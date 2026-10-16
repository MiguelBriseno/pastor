# Arquitectura

pastor no tiene un servidor propio: **tmux es el servidor**. Cada agente es una sesión de tmux y
pastor es un cliente que lanza sesiones, lee sus pantallas y guarda su propio estado en disco.

```
          ┌─────────────┐   ┌─────────────┐
          │ pastor (TUI)│   │ pastor (CLI)│
          └──────┬──────┘   └──────┬──────┘
                 │  lee/escribe     │
                 ▼                  ▼
   ~/.local/share/pastor/state.json   (agentes y tareas)
   ~/.local/share/pastor/status/*.json (estado que reportan los hooks)
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

Cada sesión se crea con `remain-on-exit on`: si el agente termina, su panel sigue visible para
leer lo último que escribió, y tmux lo marca como `pane_dead`.

### El estado se lee del disco en cada operación

`State::update` carga `state.json`, aplica el cambio y lo guarda con escritura atómica (archivo
temporal y `rename`). La TUI recarga el archivo cada segundo. Así la CLI y la TUI pueden usarse a
la vez.

### Cómo se clasifica el estado de un agente

`agent::classify` decide en este orden:

1. Si el panel está muerto → **terminado**.
2. Si en las últimas líneas de la pantalla hay un diálogo de permiso o confirmación
   (`do you want to`, `enter to confirm`…) → **esperando**.
3. Si aparece `esc to interrupt` → **trabajando**.
4. Si hay un estado de hook reciente (solo Claude Code), se usa ese.
5. Si la pantalla cambió en los últimos 3 segundos → **trabajando**; si no → **inactivo**.

Un estado de hook se considera viejo cuando la pantalla cambió después de él; esto evita que un
*esperando* quede pegado después de que respondiste.

### Hooks de Claude Code

Al lanzar Claude Code se le pasa `--settings ~/.local/share/pastor/claude-settings.json` y la
variable `PASTOR_AGENT=<id>`. Los hooks llaman a `pastor hook <evento>`, que escribe
`status/<id>.json`. Con el evento `Stop`, además, la tarea del agente pasa a *por revisar* aunque
la TUI esté cerrada.

### Envío de mensajes

`tmux::send_text` carga el texto en un buffer de tmux y lo pega con *bracketed paste*
(`paste-buffer -p`) antes de pulsar Enter. Así los mensajes de varias líneas llegan como un solo
mensaje y no se envían línea por línea.

## Cómo agregar un nuevo tipo de agente

1. Agrega la variante a `Kind` en `src/agent.rs` y a `Kind::ALL`.
2. Define su binario en `Kind::bin` y cómo recibe el primer mensaje en `Kind::launch_command`.
3. Si muestra textos propios mientras trabaja o pide permisos, agrégalos a `WORKING` o `BLOCKED`.
4. Agrega su etiqueta en el gráfico del dashboard (`draw_dashboard` en `src/tui.rs`).
