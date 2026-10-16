# pastor

**pastor** es un gestor de agentes de código para la terminal. Lanza y vigila varios agentes de
[Claude Code](https://docs.claude.com/es/docs/claude-code/overview),
[Codex](https://github.com/openai/codex) y [opencode](https://opencode.ai) a la vez, te avisa
cuando alguno necesita tu respuesta y te deja repartirles tareas.

```
 pastor    1 Agentes  │  2 Tareas  │  3 Dashboard                          ● 2  ▲ 1  ○ 1
╭ Agentes ─────────────────────────────────╮╭ api · claude · ~/proyectos/api ─────────────╮
│ ● api              claude   trabajando   ││ ● Ejecutando los tests…                      │
│ ▲ web              codex    esperando    ││                                              │
│ ○ docs             opencode inactivo     ││   ✓ 42 pruebas pasaron                       │
│ ● infra            claude   trabajando   ││                                              │
╰──────────────────────────────────────────╯╰ tarea t3: Migrar login a OAuth ──────────────╯
 ↑↓ mover · ⏎ entrar · n nuevo · s enviar mensaje · x terminar · 1-3 pestañas · q salir
```

## Características

- **Agentes persistentes.** Cada agente corre en su propia sesión de tmux. Si cierras pastor o
  tu terminal, los agentes siguen trabajando; al volver los encuentras como los dejaste.
- **Estado en vivo.** Cada agente aparece como *trabajando*, *esperando* (necesita tu
  aprobación o respuesta), *inactivo* o *terminado*.
- **Notificaciones de escritorio** cuando un agente te necesita o termina su trabajo.
- **Tareas.** Crea tareas y asígnalas a un agente existente o a uno nuevo. Cuando el agente
  termina, la tarea pasa sola a *por revisar*.
- **Dashboard** con el resumen de agentes, el progreso de las tareas y lo que necesita tu
  atención.
- **CLI completa** para usarlo desde scripts o desde otros agentes.
- **No toca tu configuración.** Usa un servidor de tmux propio y los hooks de Claude Code solo
  se aplican a los agentes que lanza pastor.

## Requisitos

| Requisito | Para qué |
|---|---|
| Linux o macOS | Windows no está soportado (se puede usar desde WSL) |
| [tmux](https://github.com/tmux/tmux) 3.0 o superior | Mantiene vivas las terminales de los agentes |
| [Rust](https://rustup.rs) 1.82 o superior | Para compilar pastor |
| Al menos un agente: `claude`, `codex` u `opencode` | Los agentes que vas a manejar; deben estar en tu `PATH` |
| `notify-send` (opcional) | Notificaciones de escritorio en Linux |

### Instalar los requisitos

**Debian / Ubuntu**

```sh
sudo apt install -y tmux build-essential libnotify-bin
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**macOS** (con [Homebrew](https://brew.sh))

```sh
brew install tmux rustup
rustup-init
```

> En macOS las notificaciones de escritorio todavía no funcionan (usan `notify-send`).

## Instalación

Directo desde GitHub:

```sh
cargo install --git https://github.com/MiguelBriseno/pastor
```

O desde una copia local del código:

```sh
git clone https://github.com/MiguelBriseno/pastor
cd pastor
cargo install --path .
```

El binario queda en `~/.cargo/bin/pastor`. Si tu terminal no lo encuentra, abre una terminal
nueva o agrega `~/.cargo/bin` a tu `PATH`.

Para comprobar que todo está listo:

```sh
pastor --version
tmux -V
which claude codex opencode
```

## Primeros pasos

1. Abre la interfaz:

   ```sh
   pastor
   ```

2. Pulsa `n` para lanzar un agente. Elige el tipo con `←` `→`, escribe el directorio del
   proyecto y, si quieres, un primer mensaje. Confirma con `⏎`.
3. El agente aparece en la lista. A la derecha ves su pantalla en vivo.
4. Pulsa `⏎` para entrar a su terminal y hablar con él como siempre. Para volver a pastor pulsa
   **`Ctrl-b` y luego `d`**.
5. Lanza más agentes y deja que trabajen. Cuando uno te necesite verás `▲ esperando` y te llegará
   una notificación.

> **La primera vez** que abres un agente en una carpeta nueva, Claude Code y Codex preguntan si
> confías en ella. pastor lo muestra como *esperando*: entra con `⏎` y responde.

## La interfaz

Cambia de pestaña con `1`, `2`, `3` o `Tab`. Sal con `q` (los agentes siguen corriendo).

### 1 · Agentes

| Tecla | Acción |
|---|---|
| `↑` `↓` / `k` `j` | Mover la selección |
| `⏎` o `a` | Entrar a la terminal del agente (volver con `Ctrl-b d`) |
| `n` | Lanzar un agente nuevo |
| `s` | Enviarle un mensaje sin entrar |
| `x` | Terminar el agente (pide confirmación) |

### 2 · Tareas

| Tecla | Acción |
|---|---|
| `n` | Nueva tarea (título y descripción; cambia de campo con `Tab`) |
| `⏎` o `a` | Asignar la tarea a un agente existente o a uno nuevo |
| `espacio` | Avanzar el estado: pendiente → en curso → por revisar → hecha |
| `d` | Marcar como hecha |
| `g` | Ir al agente que tiene la tarea |
| `x` | Borrar la tarea (pide confirmación) |

Al asignar una tarea, pastor le envía al agente el título y la descripción como mensaje, y la
marca *en curso*. Cuando el agente termina, pasa a *por revisar*.

### 3 · Dashboard

Cuántos agentes hay en cada estado, cuántas tareas en cada etapa, el porcentaje completado y una
lista de lo que necesita tu atención: agentes esperando respuesta y tareas por revisar.

### En los formularios

| Tecla | Acción |
|---|---|
| `Tab` / `↓` | Siguiente campo |
| `Shift-Tab` / `↑` | Campo anterior |
| `⏎` | Confirmar |
| `Esc` | Cancelar |
| `Ctrl-u` | Borrar el campo |

## Línea de comandos

Todo lo que hace la interfaz también se puede hacer desde la línea de comandos. Usa
`pastor <comando> --help` para ver todas las opciones.

### Agentes

```sh
pastor new claude                                   # en el directorio actual
pastor new codex --cwd ~/proyectos/web --name web   # con directorio y nombre
pastor new claude -p "arregla los tests que fallan" # con un primer mensaje

pastor ls                     # lista agentes y su estado
pastor attach web             # entra a su terminal (salir con Ctrl-b d)
pastor send web "ahora agrega pruebas para el login"
pastor kill web               # lo termina
```

Puedes referirte a un agente por su **id** (`a1`, `a2`…) o por su **nombre**. Si no le das
nombre, se usa el de la carpeta.

### Tareas

```sh
pastor task add "Migrar login a OAuth" -d "Usa el proveedor de Google. No toques el registro."
pastor task ls

pastor task assign t1 --new claude --cwd ~/proyectos/api   # con un agente nuevo
pastor task assign t1 --agent api                          # con uno que ya existe

pastor task done t1
pastor task rm t1
```

## Estados de un agente

| Icono | Estado | Significa |
|---|---|---|
| `◌` | iniciando | Se acaba de lanzar |
| `●` | trabajando | Está pensando o ejecutando herramientas |
| `▲` | esperando | Necesita que apruebes algo o respondas una pregunta |
| `○` | inactivo | Terminó y espera tu siguiente mensaje |
| `✕` | terminado | El proceso salió; su última pantalla sigue visible |

**Cómo se detecta:**

- **Claude Code:** pastor lo lanza con `--settings` y unos hooks (`UserPromptSubmit`,
  `PreToolUse`, `PostToolUse`, `Notification` y `Stop`) que le avisan de cada cambio. Es la forma
  más precisa. Estos hooks se suman a tu configuración; no la modifican.
- **Codex y opencode:** pastor lee su pantalla y busca textos como `esc to interrupt` o diálogos
  de permiso. Si una versión nueva cambia esos textos, la detección puede fallar; avísanos con un
  issue.

## Preguntas frecuentes

**¿Qué pasa si cierro pastor?**
Nada. Los agentes siguen corriendo en tmux. Al abrir `pastor` otra vez aparecen como los dejaste.

**¿Y si reinicio la computadora?**
Los agentes se pierden (tmux no sobrevive a un reinicio). Aparecerán como *terminados*; quítalos
con `x`. Restaurarlos automáticamente está en el plan.

**¿Interfiere con mi tmux?**
No. pastor usa su propio servidor (`tmux -L pastor`). Puedes verlo con `tmux -L pastor ls`.

**Estoy dentro de un agente y no sé salir.**
Pulsa `Ctrl-b` y luego `d`. Eso te saca sin cerrar al agente. No uses `/exit` a menos que quieras
terminarlo.

**¿Dónde guarda sus datos?**
En `~/.local/share/pastor/` en Linux y `~/Library/Application Support/pastor/` en macOS:

- `state.json`: agentes y tareas
- `status/`: el último estado que reportó cada agente de Claude Code
- `claude-settings.json`: los hooks que se le pasan a Claude Code

**¿Cómo desinstalo pastor?**

```sh
tmux -L pastor kill-server      # termina todos los agentes
cargo uninstall pastor
rm -rf ~/.local/share/pastor
```

## Plan

- [x] Agentes sobre tmux, CLI, interfaz, estados y notificaciones
- [x] Tareas y dashboard
- [ ] Agrupar agentes por proyecto
- [ ] Un worktree de git por agente, para trabajar en paralelo sobre el mismo repositorio
- [ ] Restaurar agentes después de reiniciar
- [ ] Integraciones con Slack y Asana
- [ ] Manejar agentes en otras máquinas por SSH

## Contribuir

Las contribuciones son bienvenidas. Lee [CONTRIBUTING.md](CONTRIBUTING.md) y, para entender el
código, [docs/arquitectura.md](docs/arquitectura.md).
