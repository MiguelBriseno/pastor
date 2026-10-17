//! Hooks de Claude Code: `pastor hook <evento>` escribe el estado del agente
//! en ~/.local/share/pastor/status/<id>.json.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent;
use crate::history::{self, Event};
use crate::ops;
use crate::store::{data_dir, ensure_dirs, write_atomic, write_atomic_mode, write_volatile};
use crate::tmux::shell_quote;

#[derive(Serialize, Deserialize, Debug)]
pub struct HookStatus {
    pub state: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub message: String,
}

fn status_path(id: &str) -> PathBuf {
    data_dir().join("status").join(format!("{id}.json"))
}

pub fn read_status(id: &str) -> Option<HookStatus> {
    serde_json::from_str(&fs::read_to_string(status_path(id)).ok()?).ok()
}

pub fn clear_status(id: &str) {
    let _ = fs::remove_file(status_path(id));
}

/// Llamado por Claude Code. Nunca debe fallar ruidosamente.
pub fn handle(event: &str) -> Result<()> {
    let Ok(id) = std::env::var("PASTOR_AGENT") else {
        return Ok(());
    };
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let message = v["message"].as_str().unwrap_or_default().to_string();

    let state = match event {
        "notify" => match v["notification_type"].as_str() {
            Some("idle_prompt") => "idle",
            Some("permission_prompt" | "elicitation_dialog") => "blocked",
            // Versiones sin `notification_type`: se mira el mensaje.
            None | Some("") if message.contains("waiting for your input") => "idle",
            None | Some("") => "blocked",
            // Otros avisos (p. ej. `auth_success`) no piden nada.
            Some(_) => return Ok(()),
        },
        "idle" => "idle",
        _ => "working",
    };
    ensure_dirs()?;
    let status = HookStatus {
        state: state.into(),
        at: Utc::now(),
        message,
    };
    write_volatile(
        &status_path(&id),
        serde_json::to_string(&status)?.as_bytes(),
    )?;

    // Claude terminó su turno: su tarea queda lista para revisar.
    if event == "idle" {
        history::record(Event::Turn { agent: id.clone() });
        ops::finish_turn(&id)?;
    }
    Ok(())
}

/// Anota en `hooks.log` un hook que falló, porque Claude Code no lo muestra.
pub fn log_error(event: &str, e: &anyhow::Error) {
    use std::io::Write;
    let agent = std::env::var("PASTOR_AGENT").unwrap_or_default();
    let line = format!("{} {agent} {event}: {e:#}\n", Utc::now().to_rfc3339());
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir().join("hooks.log"))
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Ruta de este binario para los hooks. En Linux, si se reinstaló mientras
/// corría (`cargo install` lo reemplaza), `current_exe` añade " (deleted)" a
/// una ruta en la que ya está el binario nuevo.
fn pastor_exe() -> Result<PathBuf> {
    Ok(strip_deleted(std::env::current_exe()?))
}

fn strip_deleted(exe: PathBuf) -> PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => exe,
    }
}

/// Escribe el settings.json que se pasa a `claude --settings`.
/// Se suma a tu configuración normal de Claude Code, no la reemplaza.
pub fn claude_settings() -> Result<PathBuf> {
    let exe = shell_quote(&pastor_exe()?.to_string_lossy());
    let hook = |ev: &str| json!([{ "hooks": [{ "type": "command", "command": format!("{exe} hook {ev}") }] }]);
    let tool_hook = |ev: &str| json!([{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("{exe} hook {ev}") }] }]);
    let settings = json!({
        "hooks": {
            "UserPromptSubmit": hook("working"),
            "PreToolUse": tool_hook("working"),
            "PostToolUse": tool_hook("working"),
            "Notification": hook("notify"),
            "Stop": hook("idle"),
        }
    });
    ensure_dirs()?;
    let path = data_dir().join("claude-settings.json");
    write_atomic(&path, serde_json::to_string_pretty(&settings)?.as_bytes())?;
    Ok(path)
}

/// Código de `sh` que abre el shell del usuario cuando el agente sale. Ese
/// shell tiene al inicio del PATH un `claude` que lo vuelve a abrir con los
/// hooks. El PATH se ajusta después de leer tu configuración (`.bashrc`,
/// `.zshrc`…), que suele anteponer sus propios directorios.
pub fn relaunch_shell() -> Result<String> {
    let dir = data_dir().join("shell");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin)?;
    write_claude_shim(&bin)?;
    shell_files(&dir)
}

/// Escribe en `dir` la configuración de bash y zsh que antepone `dir/bin`
/// al PATH, y devuelve el código de `sh` que abre el shell con ella.
fn shell_files(dir: &Path) -> Result<String> {
    let bin = dir.join("bin");
    let q = |p: &Path| shell_quote(&p.to_string_lossy());
    let prepend = format!("PATH={}:\"$PATH\"\n", q(&bin));
    write_atomic(
        &dir.join("bashrc"),
        format!("[ -f ~/.bashrc ] && . ~/.bashrc\n{prepend}").as_bytes(),
    )?;
    let zdot = dir.join("zsh");
    fs::create_dir_all(&zdot)?;
    // PASTOR_ZDOTDIR trae tu ZDOTDIR (vacío si no tenías). Tu `.zshenv` se
    // lee con él, y si lo cambia (p. ej. a ~/.config/zsh) lo guardamos para
    // el `.zshrc`: zsh debe seguir leyendo el `.zshrc` de aquí, o el PATH no
    // se ajusta.
    let user_zdotdir = "if [ -n \"$PASTOR_ZDOTDIR\" ]; then ZDOTDIR=\"$PASTOR_ZDOTDIR\"; \
                        else unset ZDOTDIR; fi\n";
    write_atomic(
        &zdot.join(".zshenv"),
        format!(
            "PASTOR_ZSH=\"$ZDOTDIR\"\n{user_zdotdir}\
             [ -f \"${{ZDOTDIR:-$HOME}}/.zshenv\" ] && . \"${{ZDOTDIR:-$HOME}}/.zshenv\"\n\
             PASTOR_ZDOTDIR=\"${{ZDOTDIR-}}\"\n\
             ZDOTDIR=\"$PASTOR_ZSH\"\n\
             unset PASTOR_ZSH\n"
        )
        .as_bytes(),
    )?;
    // El /etc/zshrc de macOS pone HISTFILE=${ZDOTDIR:-$HOME}/.zsh_history
    // cuando ZDOTDIR todavía es el de aquí: lo devolvemos a tu historial.
    write_atomic(
        &zdot.join(".zshrc"),
        format!(
            "PASTOR_ZSH=\"$ZDOTDIR\"\n{user_zdotdir}unset PASTOR_ZDOTDIR\n\
             [ \"$HISTFILE\" = \"$PASTOR_ZSH/.zsh_history\" ] && \
             HISTFILE=\"${{ZDOTDIR:-$HOME}}/.zsh_history\"\n\
             unset PASTOR_ZSH\n\
             [ -f \"${{ZDOTDIR:-$HOME}}/.zshrc\" ] && . \"${{ZDOTDIR:-$HOME}}/.zshrc\"\n{prepend}"
        )
        .as_bytes(),
    )?;

    Ok(format!(
        r#"sh_="${{SHELL:-/bin/sh}}"
case "${{sh_##*/}}" in
  bash) exec "$sh_" --rcfile {bashrc} -i ;;
  zsh) PASTOR_ZDOTDIR="${{ZDOTDIR-}}" ZDOTDIR={zdot} exec "$sh_" -i ;;
  fish) exec "$sh_" -i -C {fish} ;;
  *) PATH={bin}:"$PATH" exec "$sh_" -i ;;
esac"#,
        bashrc = q(&dir.join("bashrc")),
        zdot = q(&zdot),
        fish = shell_quote(&format!("set -gx PATH {} $PATH", q(&bin))),
        bin = q(&bin),
    ))
}

/// `claude` que llama al de verdad con `--settings`, para que los hooks
/// sigan funcionando cuando lo vuelves a abrir a mano. Con headroom pasa
/// primero por él. headroom busca `claude` en el PATH y vuelve a dar con
/// este script: `PASTOR_VIA_HEADROOM` evita el bucle, y `ANTHROPIC_BASE_URL`
/// (que define headroom) que tu alias `claude='headroom wrap claude'` lo
/// envuelva dos veces.
fn write_claude_shim(bin: &PathBuf) -> Result<()> {
    let real = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .filter(|d| d != bin)
            .map(|d| d.join("claude"))
            .find(|f| f.is_file())
    });
    let Some(real) = real else {
        return Ok(());
    };
    let via_headroom = agent::headroom().map_or(String::new(), |h| {
        format!(
            "[ -z \"$PASTOR_VIA_HEADROOM$ANTHROPIC_BASE_URL\" ] && \
             PASTOR_VIA_HEADROOM=1 exec {} wrap claude -- \"$@\"\n",
            shell_quote(&h.to_string_lossy())
        )
    });
    // Un `claude` que lanza el propio agente (p. ej. `claude -p` desde su
    // herramienta Bash) va sin los hooks: su fin de turno no es el del agente.
    let real = shell_quote(&real.to_string_lossy());
    let script = format!(
        "#!/bin/sh\n[ -n \"$CLAUDECODE\" ] && exec {real} \"$@\"\n\
         {via_headroom}exec {real} --settings {} \"$@\"\n",
        shell_quote(&claude_settings()?.to_string_lossy()),
    );
    write_atomic_mode(&bin.join("claude"), script.as_bytes(), Some(0o755))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quita_el_sufijo_de_binario_reemplazado() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(strip_deleted(p("/bin/pastor (deleted)")), p("/bin/pastor"));
        assert_eq!(strip_deleted(p("/bin/pastor")), p("/bin/pastor"));
    }

    /// HOME temporal que se borra al terminar, aunque falle la prueba.
    struct TempHome(PathBuf);

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Abre el shell que queda tras salir el agente, con `home` como HOME, y
    /// devuelve las líneas de `cmd` que empiezan con `pastor:` (la
    /// configuración del sistema puede imprimir otras). `None` si ese shell
    /// no está instalado.
    fn relaunch(shell: &str, home: &Path, cmd: &str) -> Option<String> {
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let sh = agent::find_in_path(shell)?;
        let dir = home.join("pastor");
        fs::create_dir_all(dir.join("bin")).unwrap();
        let script = shell_files(&dir).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", &script])
            .env("HOME", home)
            .env("SHELL", sh)
            .env_remove("ZDOTDIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Sin terminal de control: un shell interactivo podría escribir en
        // la tuya o quitársela a cargo.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(cmd.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<_> = out.lines().filter(|l| l.starts_with("pastor:")).collect();
        Some(lines.join("\n"))
    }

    #[test]
    fn el_shell_lee_tu_configuracion_y_antepone_el_claude_de_pastor() {
        let home = TempHome(std::env::temp_dir().join(format!(
            "pastor-shell-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        let home = home.0.as_path();
        fs::create_dir_all(home.join(".config/zsh")).unwrap();
        let bin = home.join("pastor/bin");
        let expected =
            |zdotdir: &str| format!("pastor: rc=si path={} zdotdir={zdotdir}", bin.display());
        let cmd = "echo \"pastor: rc=$DESDE_RC path=${PATH%%:*} zdotdir=${ZDOTDIR-no}\"\n";

        fs::write(home.join(".bashrc"), "DESDE_RC=si\n").unwrap();
        if let Some(out) = relaunch("bash", home, cmd) {
            assert_eq!(out, expected("no"), "bash");
        }

        fs::write(home.join(".zshrc"), "DESDE_RC=si\n").unwrap();
        if let Some(out) = relaunch("zsh", home, cmd) {
            // Sin ZDOTDIR antes, sin ZDOTDIR después.
            assert_eq!(out, expected("no"), "zsh");
            // Un `.zshenv` que mueve ZDOTDIR no debe saltarse nuestro `.zshrc`.
            fs::remove_file(home.join(".zshrc")).unwrap();
            fs::write(
                home.join(".zshenv"),
                "export ZDOTDIR=\"$HOME/.config/zsh\"\n",
            )
            .unwrap();
            fs::write(home.join(".config/zsh/.zshrc"), "DESDE_RC=si\n").unwrap();
            let zdotdir = home.join(".config/zsh");
            let out = relaunch("zsh", home, cmd).unwrap();
            assert_eq!(out, expected(&zdotdir.to_string_lossy()), "zsh con ZDOTDIR");
        }
    }
}
