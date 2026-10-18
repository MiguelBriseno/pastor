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
| `src/ntfy.rs` | Publica los avisos en un tema de ntfy con `curl`, si está configurado |
| `src/server.rs` | `pastor serve`, solo con el feature `serve`: API HTTP con `axum` sobre las mismas funciones de `ops` |
| `web/` | La app del celular (PWA) que sirve `pastor serve` en `/`: HTML, CSS y JS sin build, metidos en el binario |

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
la vez. Los temporales de una escritura que murió a medias los borra la limpieza de `status/` al
abrir la TUI o con `pastor ls`.

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
2. Si hay un *inactivo* de hook (solo Claude Code) cuya huella es la pantalla actual → **inactivo**.
   Va antes que los textos: el último mensaje del turno puede traer un `Do you want to…` o un
   `(y/n)` sin que haya diálogo. Sin huella que coincida mandan los textos, por si es un diálogo
   que no lanza hooks.
3. Si en las últimas líneas de la pantalla hay un diálogo de permiso o confirmación
   (`do you want to`, `enter to confirm`…) → **esperando**.
4. Si aparece `esc to interrupt` → **trabajando**.
5. Si hay otro estado de hook vigente, se usa ese. Un *esperando* del hook no cuenta si al final de
   la pantalla está el aviso de turno interrumpido (`What should Claude do instead?`): al rechazar
   un permiso con Esc, Claude Code no lanza `Stop`. Un *trabajando* solo cuenta si la pantalla
   cambió en los últimos 3 segundos.
6. Si el agente tiene menos de 5 segundos → **iniciando**. Si no, si la pantalla cambió en los
   últimos 3 segundos → **trabajando**; si no → **inactivo**.

En Claude Code, para el observador la pantalla cambia cuando cambia su huella (ver abajo): lo que
pasa en el pie, como escribir en el prompt o las pistas que pone y quita, no cuenta como actividad.
En Codex y opencode, cuyo pie es distinto, cuenta la pantalla entera.

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
`state.json`, el hook no escribe nada. Si el agente sigue vivo, el hook solo cuenta si su proceso
desciende de un panel de la sesión del agente: un `tmux list-panes` y la cadena de padres, por
`/proc` en Linux o con `ps` en otros sistemas. Un servicio que se desligó cuelga de init y no pasa,
aunque herede `PASTOR_AGENT` y `TMUX_PANE`. Si no se puede comprobar, el hook se da por bueno: tmux
o `ps` fallan, no se ve el propio proceso ni ningún panel (otro espacio de PID), o la cadena se
corta o no se acaba. La TUI y `pastor ls` borran al arrancar los estados de
agentes que ya no existen, con el candado de `state.json` tomado para no llevarse los de un agente
recién creado.

### Envío de mensajes

`tmux::send_text` carga el texto en un buffer de tmux y lo pega con *bracketed paste*
(`paste-buffer -p`) antes de pulsar Enter. Así los mensajes de varias líneas llegan como un solo
mensaje y no se envían línea por línea.

### `pastor serve` es opcional

El servidor trae `axum` y `tokio`, que pesan más que todo lo demás junto, así que va detrás del
feature `serve` y quien no lo usa no los compila. Llama a `ops` como la CLI (no lanza `pastor`),
cada operación en un hilo de `spawn_blocking` porque tocan tmux y el disco. Guarda un `Tracker`
entre consultas, así un cliente que pregunta cada pocos segundos ve *trabajando* como la TUI.
El token se compara sin cortar en el primer byte distinto, y su archivo se crea ya con permisos
0600 (`write_atomic_mode` crea el temporal con el modo, no lo cambia después).

### La app del celular va dentro del binario

`web/` es HTML, CSS y JavaScript sin framework ni paso de build, y `server.rs` lo mete en el binario
con `include_bytes!`: no hay archivos que instalar junto a `pastor` y la página siempre corresponde
a la API del mismo binario. Se sirve en el mismo origen que `/api`, así que no hace falta CORS ni hay
contenido mixto con HTTPS. La página no pide token para cargarse (sin él no puede hacer nada); lo
pide la API, y la página lo guarda en `localStorage`. Los textos de los agentes se insertan con
`textContent`, nunca como HTML. El service worker solo guarda la página para que abra sin red,
siempre pregunta primero a la red, y nunca guarda respuestas de `/api`.

### El agente dibuja al tamaño de quien lo mira

Las sesiones nacen de 200×50 y, sin nadie dentro, tmux no las cambia. Claude parte sus líneas a ese
ancho, así que la vista previa de la TUI las cortaba y en el celular había que moverse de lado.
Ajustar el texto después rompe los separadores y las tablas; en cambio, al cambiar el tamaño de su
panel el agente se redibuja a la medida.

`tmux::fit` deja el panel del agente en las columnas y filas pedidas. Cambia la ventana que lo
contiene, pero si la terminal está al lado conserva su ancho, y luego ajusta el panel justo. La
última vista que pidió un tamaño manda: la TUI lo pide cuando cambia su vista previa (cuando el
tamaño se queda quieto, en otro hilo), al volver de `attach` y cuando vuelves a usarla; la app
del celular, al abrir un agente y cuando cambia lo que cabe en su pantalla. Ninguna lo repite en
cada consulta, para que dos pantallas abiertas en el mismo agente no se lo quiten sin parar. Si
alguien está dentro de la sesión no se toca: manda su terminal. `resize-window` deja la ventana
en `window-size manual`, así que después se quita la opción para que `pastor attach` siga tomando
el tamaño de tu terminal.

### Los avisos al celular van con `curl`

ntfy recibe un POST con el texto. Para eso basta el `curl` del sistema, como `notify-send` para el
escritorio, en vez de un cliente HTTP con TLS en el binario. La URL, el token y el mensaje van por
la entrada de `curl` (`-K -`), no como argumentos, para que el token no se vea en `ps`. El hilo de
avisos lanza `notify-send` y `curl` a la vez y espera a los dos con un límite.

## Cómo agregar un nuevo tipo de agente

1. Agrega la variante a `Kind` en `src/agent.rs` y a `Kind::ALL`.
2. Define su binario en `Kind::bin` y cómo recibe el primer mensaje en `Kind::launch_command`.
3. Si muestra textos propios mientras trabaja o pide permisos, agrégalos a `WORKING` o `BLOCKED`.
4. Agrega su etiqueta en el gráfico del dashboard (`draw_dashboard` en `src/tui.rs`).
