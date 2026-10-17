# pastor

**pastor** es un gestor de agentes de código para la terminal. Lanza y vigila varios agentes de
[Claude Code](https://docs.claude.com/es/docs/claude-code/overview),
[Codex](https://github.com/openai/codex) y [opencode](https://opencode.ai) a la vez, te avisa
cuando alguno necesita tu respuesta y te deja repartirles tareas.

```
 pastor    1 Agentes  │  2 Tareas  │  3 Dashboard  │  4 Historial          ● 2  ▲ 1  ○ 1
╭ Agentes ─────────────────────────────────╮╭ api · claude · ~/proyectos/api ─────────────╮
│ ● api              claude   trabajando   ││ ● Ejecutando los tests…                      │
│ ▲ web              codex    esperando    ││                                              │
│ ○ docs             opencode inactivo     ││   ✓ 42 pruebas pasaron                       │
│ ● infra            claude   trabajando   ││                                              │
╰──────────────────────────────────────────╯╰ tarea t3: Migrar login a OAuth ──────────────╯
 ↑↓ mover · ⏎ entrar · n nuevo · s enviar mensaje · x terminar · 1-4 pestañas · q salir
```

## Características

- **Agentes persistentes.** Cada agente corre en su propia sesión de tmux. Si cierras pastor o
  tu terminal, los agentes siguen trabajando; al volver los encuentras como los dejaste.
- **Estado en vivo.** Cada agente aparece como *trabajando*, *esperando* (necesita tu
  aprobación o respuesta), *inactivo* o *terminado*.
- **Terminal al lado de cada agente.** Abre una terminal junto al agente, en su mismo directorio,
  para correr por ejemplo `npm run dev` mientras el agente hace cambios.
- **Notificaciones de escritorio** cuando un agente te necesita o termina su trabajo.
- **Tareas.** Crea tareas y asígnalas a un agente existente o a uno nuevo. Cuando el agente
  termina, la tarea pasa sola a *por revisar*.
- **Historial** desde que instalaste pastor: sesiones, tareas, mensajes, tiempos y actividad
  por día.
- **Dashboard** con el resumen de agentes, el progreso de las tareas y lo que necesita tu
  atención.
- **CLI completa** para usarlo desde scripts o desde otros agentes.
- **No toca tu configuración.** Usa un servidor de tmux propio y los hooks de Claude Code solo
  se aplican a los agentes que lanza pastor.

## Requisitos

| Requisito | Para qué |
|---|---|
| Linux o macOS | Windows no está soportado (se puede usar desde WSL) |
| [tmux](https://github.com/tmux/tmux) 3.2 o superior | Mantiene vivas las terminales de los agentes |
| [Rust](https://rustup.rs) 1.89 o superior | Para compilar pastor |
| Al menos un agente: `claude`, `codex` u `opencode` | Los agentes que vas a manejar; deben estar en tu `PATH` |
| `notify-send` (opcional) | Notificaciones de escritorio en Linux; en macOS se usan las del sistema |
| Una [Nerd Font](https://www.nerdfonts.com) (recomendada) | Iconos en la interfaz; sin ella usa `PASTOR_ICONS=simple` (ver [Iconos y fuente](#iconos-y-fuente)) |

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

> En macOS pastor envía las notificaciones con `osascript`, así que suelen aparecer como de
> **Script Editor**. Si no ves ninguna, revisa que estén activadas para esa app en *Ajustes del
> Sistema → Notificaciones*.

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

Cambia de pestaña con `1`, `2`, `3`, `4` o `Tab`. Sal con `q` (los agentes siguen corriendo).

### 1 · Agentes

| Tecla | Acción |
|---|---|
| `↑` `↓` / `k` `j` | Mover la selección |
| `⏎` o `a` | Entrar a la terminal del agente (volver con `Ctrl-b d`) |
| `n` | Lanzar un agente nuevo |
| `s` | Enviarle un mensaje sin entrar |
| `t` | Abrir una terminal al lado del agente (ver abajo) |
| `T` | Cerrar esa terminal |
| `x` | Terminar el agente (pide confirmación) |
| `<` `>` | Estrechar / ensanchar la lista de agentes |
| `[` `]` | Ensanchar / estrechar la terminal de al lado |

#### Terminal al lado del agente

Pulsa `t` sobre un agente para abrir una terminal a su derecha, en el mismo directorio del
proyecto. pastor te propone un comando para ejecutar en ella:

- el último que usaste con ese agente, o
- si el proyecto tiene un script `dev` en `package.json`, `npm run dev` (o `pnpm dev`, `yarn dev`,
  `bun dev`, según el lockfile que encuentre).

Puedes cambiarlo o dejarlo vacío para abrir una terminal sin comando. Al confirmar:

- En la lista, el agente muestra el icono de terminal (`⊞` en modo simple).
- La vista previa se divide en dos: el agente a la izquierda y la terminal a la derecha.
- Al entrar con `⏎` ves los dos paneles lado a lado. El foco empieza en el agente; cambia de
  panel con **`Ctrl-b` y una flecha** (o con el ratón).

La terminal es un shell normal: `Ctrl-c` detiene el servidor y puedes escribir otros comandos.
Pulsa `T` para cerrarla, o escribe `exit` en ella: el panel se cierra solo y puedes abrir otra
con `t`.

### 2 · Tareas

| Tecla | Acción |
|---|---|
| `n` | Nueva tarea (título y descripción; cambia de campo con `Tab`) |
| `⏎` o `a` | Asignar la tarea a un agente existente o a uno nuevo |
| `espacio` | Avanzar el estado: pendiente → en curso → por revisar → hecha |
| `d` | Marcar como hecha |
| `g` | Ir al agente que tiene la tarea |
| `x` | Borrar la tarea (pide confirmación) |
| `<` `>` | Agrandar / achicar el panel de descripción |

Al asignar una tarea, pastor le envía al agente el título y la descripción como mensaje, y la
marca *en curso*. Cuando el agente termina, pasa a *por revisar*.

### 3 · Dashboard

Cuántos agentes hay en cada estado, cuántas tareas en cada etapa, el porcentaje completado y una
lista de lo que necesita tu atención: agentes esperando respuesta y tareas por revisar.

### 4 · Historial

Lo que ha pasado desde que instalaste pastor: sesiones de agentes, tareas creadas y terminadas,
mensajes enviados, turnos de Claude Code, la duración media de una sesión y de una tarea, la
actividad de los últimos 14 días, las sesiones por tipo de agente y los proyectos con más
sesiones.

El historial empezó a guardarse en esta versión. De antes solo se conocen los totales de agentes
y tareas creados, que se suman a las cifras.

### En los formularios

| Tecla | Acción |
|---|---|
| `Tab` / `↓` | Siguiente campo |
| `Shift-Tab` / `↑` | Campo anterior |
| `⏎` | Confirmar |
| `Esc` | Cancelar |
| `Ctrl-u` | Borrar el campo |

Puedes pegar texto con varias líneas: en los campos de mensaje y descripción se conservan los
saltos de línea, y en los demás se convierten en espacios. Pegar nunca confirma el formulario.

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
pastor term web npm run dev   # abre una terminal al lado y ejecuta el comando
pastor term web               # abre una terminal sin comando
pastor term web --close       # la cierra
pastor kill web               # lo termina (y su terminal)
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

## Iconos y fuente

pastor usa iconos de [Nerd Font](https://www.nerdfonts.com): una oveja como logo, un icono por
tipo de agente, carpeta, terminal, estados de las tareas, etc. Los agentes que están trabajando
muestran un spinner animado.

La fuente no la elige pastor sino tu emulador de terminal. Para ver los iconos:

1. Descarga una Nerd Font (por ejemplo *JetBrainsMono Nerd Font*) de
   [nerdfonts.com/font-downloads](https://www.nerdfonts.com/font-downloads) e instálala
   (en Linux, copia los `.ttf` a `~/.local/share/fonts` y ejecuta `fc-cache -f`).
2. Selecciónala como fuente de tu terminal. En kitty, por ejemplo, en `~/.config/kitty/kitty.conf`:

   ```
   font_family JetBrainsMono Nerd Font
   ```

Si no quieres instalar una Nerd Font y ves cuadritos (`□`) en lugar de iconos, usa el modo simple,
con símbolos que muestra cualquier fuente:

```sh
export PASTOR_ICONS=simple   # agrégalo a tu ~/.bashrc o ~/.zshrc
```

## Estados de un agente

Iconos del modo simple; con Nerd Font cada estado tiene su propio icono, y *trabajando* se
muestra con un spinner animado.

| Icono | Estado | Significa |
|---|---|---|
| `◌` | iniciando | Se acaba de lanzar |
| `●` | trabajando | Está pensando o ejecutando herramientas |
| `▲` | esperando | Necesita que apruebes algo o respondas una pregunta |
| `○` | inactivo | Terminó y espera tu siguiente mensaje |
| `✕` | terminado | El agente salió (por ejemplo con `/exit`); en su panel queda una terminal donde puedes volver a abrirlo |

**Cómo se detecta:**

- **Claude Code:** pastor lo lanza con `--settings` y unos hooks (`UserPromptSubmit`,
  `PreToolUse`, `PostToolUse`, `Notification` y `Stop`) que le avisan de cada cambio. Es la forma
  más precisa. Estos hooks se suman a tu configuración; no la modifican.
- **Codex y opencode:** pastor lee su pantalla y busca textos como `esc to interrupt` o diálogos
  de permiso. Si una versión nueva cambia esos textos, la detección puede fallar; avísanos con un
  issue.

  Con Codex y opencode, el paso automático de la tarea a *por revisar* lo hace la interfaz, así
  que solo ocurre mientras `pastor` está abierto. Con Claude Code ocurre siempre, gracias a los
  hooks.

> **opencode no pide permisos por defecto:** ejecuta comandos y edita archivos sin preguntar, así
> que nunca lo verás *esperando* por una aprobación. Si quieres que pregunte, agrega a tu
> `opencode.json` algo como `"permission": { "bash": "ask", "edit": "ask" }`.

## Preguntas frecuentes

**¿Qué pasa si cierro pastor?**
Nada. Los agentes siguen corriendo en tmux. Al abrir `pastor` otra vez aparecen como los dejaste.

**¿Y si reinicio la computadora?**
Los agentes se pierden (tmux no sobrevive a un reinicio). Aparecerán como *terminados*; quítalos
con `x`. Restaurarlos automáticamente está en el plan.

**¿Interfiere con mi tmux?**
No. pastor usa su propio servidor (`tmux -L pastor`). Puedes verlo con `tmux -L pastor ls`. Ese
servidor lee tu `~/.tmux.conf`, así que conserva tu prefijo y tus atajos.

**Estoy dentro de un agente y no sé salir.**
Pulsa `Ctrl-b` y luego `d`. Eso te saca sin cerrar al agente. Si usas `/exit`, el agente se cierra
y su panel se queda con una terminal en el mismo directorio.

Si cambiaste el prefijo en tu `~/.tmux.conf`, usa el tuyo en vez de `Ctrl-b`; la barra de abajo
muestra el que corresponde. Si abriste pastor dentro de otro tmux con el mismo prefijo, el de fuera
se lo queda: púlsalo dos veces (`Ctrl-b Ctrl-b d`).

**Cerré el agente con `/exit`. ¿Cómo lo vuelvo a abrir?**
Entra con `⏎` y escribe `claude`, `codex` u `opencode` en la terminal que quedó. Claude Code se
abre otra vez con los hooks de pastor, así que su estado se sigue viendo bien. Mientras el agente
esté cerrado, pastor no le envía mensajes ni tareas, para que no se ejecuten como comandos.

**Uso [headroom](https://github.com/headroomlabs-ai/headroom). ¿pastor lo usa?**
Sí. Si `headroom` está en tu `PATH`, pastor lanza Claude Code con `headroom wrap claude`, también
cuando lo vuelves a abrir a mano con `claude` tras un `/exit`. Un alias como
`alias claude='headroom wrap claude'` no basta: pastor lanza los agentes sin pasar por tu shell, así
que el alias no aplica. Para usar Claude Code sin headroom, pon `PASTOR_HEADROOM=0`.

**¿Cómo paso del agente a su terminal?**
Dentro de la sesión, pulsa `Ctrl-b` y luego la flecha `→` o `←`. También puedes hacer clic en el
panel.

**¿Dónde guarda sus datos?**
En `~/.local/share/pastor/` en Linux y `~/Library/Application Support/pastor/` en macOS:

- `state.json`: agentes y tareas
- `history.jsonl`: el historial de eventos para la pestaña Historial (puedes borrarlo para
  empezar de cero)
- `status/`: el último estado que reportó cada agente de Claude Code
- `claude-settings.json`: los hooks que se le pasan a Claude Code

**¿Cómo desinstalo pastor?**

```sh
tmux -L pastor kill-server      # termina todos los agentes
cargo uninstall pastor
rm -rf ~/.local/share/pastor                     # Linux
rm -rf ~/Library/Application\ Support/pastor    # macOS
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

## Licencia

pastor se distribuye bajo la [licencia MIT](LICENSE): puedes usarlo, copiarlo, modificarlo y
distribuirlo libremente, siempre que conserves el aviso de copyright. El texto de la licencia está
en inglés porque es la versión oficial con validez legal.
