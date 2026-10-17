# Cómo contribuir

¡Gracias por querer mejorar pastor! Esta guía explica cómo preparar tu entorno y enviar cambios.

## Preparar el entorno

Necesitas los mismos requisitos que para usarlo (ver el [README](README.md#requisitos)).

```sh
git clone https://github.com/MiguelBriseno/pastor
cd pastor
cargo build
./target/debug/pastor
```

## Probar sin tocar tus datos

pastor guarda su estado en `$XDG_DATA_HOME/pastor`. Para hacer pruebas sin mezclar con tus
agentes reales, usa un directorio temporal:

```sh
export XDG_DATA_HOME=$(mktemp -d)
./target/debug/pastor new claude --cwd /tmp
./target/debug/pastor ls
```

Ten en cuenta que el servidor de tmux (`tmux -L pastor`) sí es compartido. Al terminar, limpia
los agentes de prueba con `pastor kill <id>`.

Para probar la interfaz sin abrirla en tu terminal, puedes correrla dentro de otro servidor de
tmux y capturar la pantalla:

```sh
tmux -L prueba new-session -d -x 150 -y 40 ./target/debug/pastor
tmux -L prueba send-keys 3            # ir al dashboard
tmux -L prueba capture-pane -p
tmux -L prueba kill-server
```

## Antes de enviar un cambio

```sh
cargo fmt
cargo clippy --all-targets   # no debe haber advertencias
cargo test
```

La CI corre lo mismo en Linux y macOS en cada pull request. Aun así, prueba a mano lo que cambiaste con al menos un agente real.

## Estilo

- Todo lo que ve el usuario va **en español**: textos de la interfaz, mensajes de error, ayuda
  de la CLI y documentación.
- Los comentarios del código también van en español.
- Los nombres en el código (variables, funciones, tipos) van en inglés.
- Mantén las funciones pequeñas y reutiliza `ops.rs` para cualquier acción que deban compartir la
  CLI y la TUI.

## Enviar el cambio

1. Crea una rama: `git checkout -b mi-mejora`.
2. Haz commits pequeños con mensajes claros en español.
3. Abre un pull request explicando qué cambia y cómo lo probaste.

## Reportar errores

Abre un issue con:

- Qué hiciste, qué esperabas y qué pasó.
- Tu sistema operativo y las versiones de `pastor --version`, `tmux -V` y del agente
  (`claude --version`, `codex --version`, `opencode --version`).
- Si el problema es la detección de estado, copia lo que muestra la pantalla del agente:
  `tmux -L pastor capture-pane -p -t <id>`.
