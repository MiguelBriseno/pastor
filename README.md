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
- **Notificaciones de escritorio** cuando un agente te necesita o termina su trabajo, y también
  en el celular con [ntfy](https://ntfy.sh) si lo configuras.
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
pastor new codex --prompt="-v revisa el diff"       # si el texto empieza por "-", con "="

pastor ls                     # lista agentes y su estado
pastor attach web             # entra a su terminal (salir con Ctrl-b d)
pastor send web "ahora agrega pruebas para el login"
pastor screen web             # lo que se ve ahora en su panel
pastor keys web Down Enter    # pulsa teclas, para contestar sus menús
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

## Desde el celular (`pastor serve`)

Una API HTTP para ver y manejar los agentes desde otro dispositivo. Es opcional: hay que
compilarla con el feature `serve`.

```sh
cargo install --path . --features serve
pastor serve                          # en 127.0.0.1:7433
pastor serve --bind 100.101.102.103:7433   # en la IP de Tailscale, para el celular
```

Todo pide un token (`Authorization: Bearer <token>`). Si no defines `PASTOR_TOKEN`, pastor genera
uno y lo guarda en `~/.local/share/pastor/serve-token`; no se pasa como argumento porque se vería
en `ps`. Controlar un agente equivale a ejecutar comandos en tu máquina: no lo expongas a internet.
El tráfico va sin cifrar, así que pastor te avisa si escuchas en algo que no sea `127.0.0.1` o una
IP de Tailscale (que ya cifra).

| Método | Ruta | Cuerpo |
|---|---|---|
| `GET` | `/api/agents` | — (cada agente trae `status`: `starting`, `working`, `blocked`, `idle` o `dead`) |
| `POST` | `/api/agents` | `{"kind": "claude", "cwd": "/ruta", "name"?, "prompt"?}` |
| `POST` | `/api/agents/{id}/send` | `{"text": "..."}` (lo pega y pulsa Enter) |
| `POST` | `/api/agents/{id}/keys` | `{"keys": ["Down", "Enter"]}` |
| `GET` | `/api/agents/{id}/screen` | — (responde `{"text": "..."}`, lo que se ve en su panel; con `?cols=48&rows=30` antes lo ajusta a ese tamaño) |
| `GET` | `/api/agents/{id}/conversation` | — (la conversación entera de un agente de Claude, de su transcripción; con `?session=…&after=N`, solo lo nuevo) |
| `DELETE` | `/api/agents/{id}` | — |
| `GET` | `/api/tasks` | — |
| `POST` | `/api/tasks` | `{"title": "...", "description"?}` |
| `POST` | `/api/tasks/{id}/assign` | `{"agent": "web"}` o `{"kind": "claude", "cwd": "/ruta"}` |
| `POST` | `/api/tasks/{id}/done` | — |
| `DELETE` | `/api/tasks/{id}` | — |
| `GET` | `/api/stats` | — (las estadísticas de la pestaña Historial; las duraciones van en segundos) |

```sh
curl -H "Authorization: Bearer $(cat ~/.local/share/pastor/serve-token)" localhost:7433/api/agents
```

`keys` (como `pastor keys`) es para los menús y diálogos que no se contestan con texto, como los permisos de Claude.
Acepta `Enter`, `Escape`, `Tab`, `BTab` (Shift-Tab), `Space`, `BSpace`, las flechas (`Up`, `Down`,
`Left`, `Right`), `Home`, `End`, `PageUp`, `PageDown`, Ctrl con una letra (`C-c`) y una letra o
cifra suelta (`1`, `y`). Ctrl solo puede ir al final: `C-c` puede terminar al agente, y lo que
viniera después llegaría al shell.

`cwd` tiene que ser una ruta absoluta. Los errores llegan como `{"error": "..."}`: 401 sin token,
404 si el agente o la tarea no existen y 400 para lo demás.

### La app del celular

`pastor serve` también sirve una app web en `/`, pensada para el celular: los agentes agrupados por
estado (los que te esperan, primero), la pantalla de cada uno en vivo con una caja para escribirle y
botones para las teclas (Esc, ⏎, flechas, Tab, `1`/`2`/`3`, `y`/`n`, Ctrl-C), la conversación entera
con los agentes de Claude, las tareas, y el dashboard y el historial como en la TUI. La
primera vez pide el token y lo guarda solo en ese dispositivo.

Para instalarla como app (PWA) el navegador exige HTTPS. Con Tailscale basta con publicar el puerto
dentro de tu tailnet, sin abrir nada a internet. En la consola de Tailscale (*DNS*) tienen que estar
activados *MagicDNS* y *HTTPS Certificates*; luego, una sola vez:

```sh
sudo tailscale serve --bg 7433            # https://<máquina>.<tailnet>.ts.net → 127.0.0.1:7433
tailscale serve status                    # la URL exacta
```

Tailscale guarda esa configuración y la mantiene después de reiniciar. Abre la URL en el celular
(Chrome en Android), pega el token y elige *Instalar app* en el menú. El tráfico va cifrado y
`pastor serve` sigue escuchando solo en `127.0.0.1`. Para quitarlo: `sudo tailscale serve --https=443 off`.

### Que `pastor serve` arranque con la computadora

Como servicio de systemd del usuario, el servidor no depende de tener pastor abierto, arranca al
encender la computadora y se reinicia si falla. Crea `~/.config/systemd/user/pastor-serve.service`:

```ini
[Unit]
Description=pastor serve: API y app del celular

[Service]
# Los agentes que se lanzan desde el celular heredan este PATH: tiene que
# encontrar claude, codex u opencode (y node, si lo usan).
Environment=PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
WorkingDirectory=%h
ExecStart=%h/.cargo/bin/pastor serve
Restart=on-failure
RestartSec=3
# Que al reiniciar el servicio solo se detenga serve, no los agentes.
KillMode=process

[Install]
WantedBy=default.target
```

Ajusta `PATH` a donde tengas los agentes (`command -v claude codex opencode`). systemd no lee tu
`.bashrc`, y si es el servidor el que arranca el tmux de pastor, los agentes heredan este entorno.

`KillMode=process` es importante por lo mismo: si fue el servidor el que arrancó el tmux de pastor,
ese tmux y todos los agentes quedan dentro del servicio, y sin esa línea `systemctl --user restart
pastor-serve` (por ejemplo, después de reinstalar) los cierra todos. Si ya tenías el servicio sin
ella, agrégala y corre `systemctl --user daemon-reload`; no hace falta reiniciar.

```sh
systemctl --user daemon-reload
systemctl --user enable --now pastor-serve
loginctl enable-linger "$USER"            # que arranque al encender, sin iniciar sesión

systemctl --user status pastor-serve      # si está corriendo
journalctl --user -u pastor-serve -f      # su registro
systemctl --user restart pastor-serve     # después de reinstalar pastor
```

### Avisos en el celular con ntfy

Los mismos avisos que salen en el escritorio («web te espera», «web terminó») pueden llegar al
celular con la app de [ntfy](https://ntfy.sh). No hace falta el feature `serve`: basta con `curl`
y definir el tema antes de abrir pastor.

```sh
export PASTOR_NTFY_URL=https://ntfy.sh/un-tema-largo-y-dificil-de-adivinar
export PASTOR_NTFY_TOKEN=tk_...   # solo si el tema pide token (servidor propio o ntfy Pro)
pastor
```

En ntfy.sh cualquiera que conozca el nombre del tema lee sus avisos, así que elige uno que no se
pueda adivinar. Los avisos los manda la interfaz o `pastor serve`: basta con que uno de los dos
esté abierto. Con los dos abiertos avisa solo la interfaz, así que no llegan dobles; al cerrarla,
sigue `pastor serve` (lo que pase en ese segundo no avisa). Como servicio de systemd, define las variables con
`Environment=PASTOR_NTFY_URL=…` en la unidad: no lee tu `.bashrc`. El que avisa también pasa a revisión la tarea de un agente que
termina. Si un aviso no sale (URL o token mal puestos, sin red), el motivo queda en `hooks.log`.

### Sonido

Cada aviso también suena en la computadora: un sonido cuando un agente te espera y otro cuando
termina (los de freedesktop con `pw-play` o `paplay`; en macOS, `afplay`). Lo hace el mismo
proceso que manda los avisos, así que tampoco suena doble. Para silenciarlo, `PASTOR_SOUND=0`.

En el celular suena la notificación de ntfy, si la configuraste; sin ntfy, el celular no suena. La
app de pastor no hace sonar nada aparte.

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

- `state.json`: agentes y tareas. `state.json.bak` guarda la versión anterior al último cambio:
  si `state.json` se corrompe (p. ej. al editarlo a mano), cópialo encima
- `hooks.log`: errores de los hooks y de los avisos de ntfy
- `avisos.lock` y `tui.lock`: quién manda los avisos (la TUI si hay una abierta, si no `pastor serve`)
- `history.jsonl`: el historial de eventos para la pestaña Historial. Si lo borras con pastor
  cerrado, empieza de nuevo pero conserva los totales de agentes y tareas que ya había
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
- [ ] Desde el celular: API, avisos con ntfy, pantalla en vivo y una PWA (hecho), y una terminal
  interactiva

## Contribuir

Las contribuciones son bienvenidas. Lee [CONTRIBUTING.md](CONTRIBUTING.md) y, para entender el
código, [docs/arquitectura.md](docs/arquitectura.md).

## Licencia

pastor se distribuye bajo la [licencia MIT](LICENSE): puedes usarlo, copiarlo, modificarlo y
distribuirlo libremente, siempre que conserves el aviso de copyright. El texto de la licencia está
en inglés porque es la versión oficial con validez legal.
