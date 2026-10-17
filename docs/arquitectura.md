# Arquitectura

pastor no tiene un servidor propio: **tmux es el servidor**. Cada agente es una sesión de tmux y
pastor es un cliente que lanza sesiones, lee sus pantallas y guarda su propio estado en disco.

Las rutas son las de Linux; en macOS `~/.local/share/pastor` es
`~/Library/Application Support/pastor`.

```
          ┌─────────────┐   ┌─────────────┐
          │ pastor (TUI)│   │ pastor (CLI)│
          └──────┬──────┘   └──────┬──────┘
                 │  lee/escribe     │
                 ▼                  ▼
   ~/.local/share/pastor/state.json   (agentes y tareas)
   ~/.local/share/pastor/history.jsonl (eventos para el historial)
   ~/.local/share/pastor/status/*.json (estado que reportan los hooks)
   ~/.local/share/pastor/hooks.log     (errores de los hooks, que Claude Code no muestra)
                 │
                 │ tmux -L pastor  (new-session, capture-pane, paste-buffer…)
                 ▼
   ┌──────────────── servidor tmux "pastor" ─────────────────┐
   │  sesión a1: claude --settings …   ──hooks──▶ pastor hook │
   │             └ panel "term": npm run dev                  │
   │  sesión a2: codex                                        │
   │  sesión a3: opencode                                     │
   └──────────────────────────────────────────────────────────┘
```

## Módulos

| Archivo | Responsabilidad |
|---|---|
| `src/main.rs` | Define la CLI con `clap` y traduce su ayuda al español |
| `src/tui.rs` | La interfaz con `ratatui`: pestañas, formularios, dibujo y teclado |
| `src/ops.rs` | Operaciones compartidas por la CLI y la TUI: lanzar, enviar, terminar, asignar tareas, observar agentes |
| `src/agent.rs` | Tipos de agente, su comando de arranque y la clasificación de estado |
| `src/hooks.rs` | Recibe los hooks de Claude Code y genera el `settings.json` que se le pasa |
| `src/tmux.rs` | Capa fina sobre el binario `tmux` |
| `src/store.rs` | Modelo de datos (`Agent`, `Task`) y persistencia en `state.json` |
| `src/history.rs` | Historial de eventos en `history.jsonl` y las estadísticas de la pestaña Historial |

## Decisiones importantes

### Un socket de tmux propio

Todos los comandos usan `tmux -L pastor`, así las sesiones de los agentes no se mezclan con las
del usuario. Las sesiones se llaman como el id del agente (`a1`, `a2`…).

### Los paneles se marcan por rol

Una sesión puede tener dos paneles: el del agente y, si se abrió, la terminal secundaria. Cada
panel lleva la opción de tmux `@pastor_role` con el valor `agent` o `term`, y pastor les habla
por su id de panel (`%3`, `%7`…), nunca por el nombre de la sesión. Así, aunque dejes el foco en
la terminal, los mensajes y la lectura de pantalla siempre van al agente. Los agentes creados
antes de existir las marcas se tratan como si su primer panel fuera el agente.

Abrir la terminal cambia el ancho del panel del agente y lo obliga a redibujarse. Para que eso no
parezca actividad, el `Tracker` guarda también el tamaño del panel y no cuenta como cambio un
redibujo causado por un cambio de tamaño.

### Al salir el agente queda un shell

El panel del agente ejecuta `sh -c '<agente>; exec $SHELL'`. Si el agente termina (por ejemplo con
`/exit`), en su panel queda el shell del usuario y desde ahí se puede volver a abrir. pastor lo
sabe por el proceso en primer plano (`pane_current_command`). Mientras corre el primer agente es el
`sh` que lo lanzó, porque comparte grupo de procesos con él; cuando el agente sale, ese `sh` marca
el panel con `@pastor_exited`. Lo que se abre después desde el shell interactivo sí queda en primer
plano, así que el agente cuenta como vivo solo si ahí está `claude`, `codex`, `opencode`, `node` o
`bun`. Si no, lo muestra como **terminado**, borra el estado de sus hooks y no le pega mensajes,
porque se ejecutarían como comandos.

Ese shell lleva al inicio del PATH `~/.local/share/pastor/shell/bin`, donde hay un `claude` que
llama al real con `--settings`. Así, si lo vuelves a abrir a mano, los hooks siguen funcionando. El
PATH se ajusta después de leer la configuración del usuario (`--rcfile` en bash, `ZDOTDIR` en zsh,
`-C` en fish), porque el `.bashrc` suele anteponer sus propios directorios.

En zsh, el `.zshenv` de pastor lee el tuyo con tu `ZDOTDIR` de verdad. Si tu `.zshenv` lo cambia
(por ejemplo a `~/.config/zsh`), pastor recuerda el nuevo valor y vuelve a poner el suyo, para que
zsh siga leyendo su `.zshrc`; ese `.zshrc` restaura tu `ZDOTDIR` y carga el tuyo.

Además, cada sesión se crea con `remain-on-exit on`: si hasta el shell termina, el panel sigue
visible para leer lo último que escribió, y tmux lo marca como `pane_dead`. La terminal
secundaria lo desactiva en su propio panel (`set-option -p`), así que al hacer `exit` se cierra sola.

### El estado se lee del disco en cada operación

`State::update` carga `state.json`, aplica el cambio y lo guarda con escritura atómica (archivo
temporal y `rename`). La TUI recarga el archivo cada segundo. Así la CLI y la TUI pueden usarse a
la vez.

### El historial solo crece

`state.json` olvida los agentes terminados y las tareas borradas, así que el historial va aparte:
`history.jsonl` tiene una línea JSON por evento (agente iniciado o terminado, tarea creada,
cambio de estado o borrada, mensaje enviado y turno de Claude Code). Cada evento se agrega con una
sola escritura en modo *append*, así que la CLI, la TUI y los hooks pueden anotar a la vez.

La primera línea es un evento `baseline` que se escribe al arrancar pastor si el archivo no existe.
Guarda los contadores `next_agent` y `next_task` de `state.json`, que nunca bajan, para contar lo
que ya había antes de llevar historial. La TUI lee solo las líneas nuevas cuando el archivo
crece y las suma a unos totales sin guardarlas: contadores, los proyectos, los ids de las tareas
hechas (para contar una sola vez las que se reabren) y la actividad de los días del gráfico. Las
estadísticas se recalculan cuando llega algo nuevo y al cambiar de día. Las líneas que no entiende
se ignoran.

### Cómo se clasifica el estado de un agente

`agent::classify` decide en este orden:

1. Si el panel está muerto, o el agente salió y en primer plano no hay un agente → **terminado**.
2. Si en las últimas líneas de la pantalla hay un diálogo de permiso o confirmación
   (`do you want to`, `enter to confirm`…) → **esperando**.
3. Si aparece `esc to interrupt` → **trabajando**.
4. Si hay un estado de hook reciente (solo Claude Code), se usa ese. Un *esperando* del hook no
   cuenta si al final de la pantalla está el aviso de turno interrumpido (`What should Claude do
   instead?`): al rechazar un permiso con Esc, Claude Code no lanza `Stop`.
5. Si la pantalla cambió en los últimos 3 segundos → **trabajando**; si no → **inactivo**.

Un estado de hook se considera viejo cuando la pantalla cambió después de él; esto evita que un
*esperando* quede pegado después de que respondiste. Hay dos formas de saberlo:

- El observador (la TUI) vio cambiar la pantalla después del hook.
- Tras un `blocked` o un `idle`, el hook lanza `pastor hook settle` en segundo plano. Ese proceso
  espera a que la pantalla deje de cambiar (tres capturas iguales seguidas) y guarda su huella en
  `status/<id>.json`: hash FNV-1a y tamaño del panel. La huella no incluye las últimas 5 líneas
  (la caja del prompt y el pie, donde Claude Code pone y quita pistas solo) y trata como espacio el
  `●` (o `⏺` en macOS) que parpadea mientras una herramienta espera permiso. Si la pantalla actual es otra, el hook es viejo aunque nadie la viera
  cambiar, como en `pastor ls` o una TUI recién abierta. Con otro tamaño de panel no se compara.

  El compromiso: cualquier otro cambio encima del pie con el mismo tamaño descarta el hook, también
  un redibujo (abrir y cerrar la terminal secundaria). Para *inactivo* da igual,
  porque el respaldo por tiempo también da inactivo. Un *esperando* de un diálogo cuyo texto no
  reconoce la lista de `BLOCKED` (p. ej. uno de MCP) puede pasar a *inactivo*. Se prefiere eso a
  un *esperando* que no se va nunca.

### Hooks de Claude Code

Al lanzar Claude Code se le pasa `--settings ~/.local/share/pastor/claude-settings.json` y la
variable `PASTOR_AGENT=<id>`. Los hooks llaman a `pastor hook <evento>`, que escribe
`status/<id>.json`. Con el evento `Stop`, además, la tarea del agente pasa a *por revisar* aunque
la TUI esté cerrada.

Un proceso que se arrancó desde un agente hereda su `PASTOR_AGENT`; si lanza sus propios `claude`
(p. ej. un servicio en segundo plano), sus hooks llegan con ese id. Si el agente ya no está en
`state.json`, el hook no escribe nada. La TUI y `pastor ls` borran al arrancar los estados de
agentes que ya no existen, con el candado de `state.json` tomado para no llevarse los de un agente
recién creado.

### Envío de mensajes

`tmux::send_text` carga el texto en un buffer de tmux y lo pega con *bracketed paste*
(`paste-buffer -p`) antes de pulsar Enter. Así los mensajes de varias líneas llegan como un solo
mensaje y no se envían línea por línea.

## Cómo agregar un nuevo tipo de agente

1. Agrega la variante a `Kind` en `src/agent.rs` y a `Kind::ALL`.
2. Define su binario en `Kind::bin` y cómo recibe el primer mensaje en `Kind::launch_command`.
3. Si muestra textos propios mientras trabaja o pide permisos, agrégalos a `WORKING` o `BLOCKED`.
4. Agrega su etiqueta en el gráfico del dashboard (`draw_dashboard` en `src/tui.rs`).
