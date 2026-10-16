//! Capa fina sobre tmux. Usamos un socket propio (`-L pastor`) para no
//! mezclarnos con las sesiones de tmux del usuario.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

const SOCKET: &str = "pastor";

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", SOCKET]);
    c
}

fn run(args: &[&str]) -> Result<String> {
    let out = tmux()
        .args(args)
        .output()
        .context("no pude ejecutar tmux")?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn ensure_available() -> Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        bail!("tmux no está instalado (sudo apt install tmux)");
    }
    Ok(())
}

/// Crea una sesión desacoplada que ejecuta `cmd` en `cwd`.
pub fn new_session(name: &str, cwd: &Path, env: &[(&str, &str)], cmd: &[String]) -> Result<()> {
    let cwd = cwd.to_string_lossy();
    let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let shell_cmd = cmd
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    let mut args = vec![
        "new-session",
        "-d",
        "-s",
        name,
        "-c",
        &cwd,
        "-x",
        "200",
        "-y",
        "50",
    ];
    for e in &env {
        args.extend(["-e", e.as_str()]);
    }
    args.push(&shell_cmd);
    run(&args)?;

    // Si el agente termina, dejamos el panel visible para poder leer su salida.
    run(&["set-option", "-w", "-t", name, "remain-on-exit", "on"])?;
    run(&["set-option", "-g", "focus-events", "on"])?;
    for (opt, val) in [
        ("mouse", "on"),
        ("status-style", "bg=colour236,fg=colour250"),
        ("status-left", " pastor:#S "),
        ("status-left-length", "30"),
        ("status-right", " Ctrl-b d → volver a pastor "),
        ("status-right-length", "40"),
    ] {
        run(&["set-option", "-t", name, opt, val])?;
    }
    Ok(())
}

pub struct PaneInfo {
    pub dead: bool,
}

/// Paneles vivos por nombre de sesión. Vacío si el servidor no está corriendo.
pub fn panes() -> HashMap<String, PaneInfo> {
    let Ok(out) = run(&["list-panes", "-a", "-F", "#{session_name}\t#{pane_dead}"]) else {
        return HashMap::new();
    };
    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(s, d)| (s.to_string(), PaneInfo { dead: d == "1" }))
        .collect()
}

/// Texto visible del panel.
pub fn capture(name: &str) -> Result<String> {
    run(&["capture-pane", "-p", "-J", "-t", name])
}

/// Pega `text` en el agente (bracketed paste) y pulsa Enter.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    let buf = format!("pastor-{name}");
    let mut child = tmux()
        .args(["load-buffer", "-b", &buf, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("no pude ejecutar tmux")?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        bail!("tmux load-buffer falló");
    }
    run(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", name])?;
    thread::sleep(Duration::from_millis(200));
    run(&["send-keys", "-t", name, "Enter"])?;
    Ok(())
}

/// Se engancha a la sesión en la terminal actual (bloquea hasta Ctrl-b d).
pub fn attach(name: &str) -> Result<()> {
    let status = tmux()
        .args(["attach-session", "-t", name])
        .env_remove("TMUX")
        .status()
        .context("no pude ejecutar tmux")?;
    if !status.success() {
        bail!("no pude entrar a la sesión {name}");
    }
    Ok(())
}

pub fn kill_session(name: &str) -> Result<()> {
    run(&["kill-session", "-t", name]).map(|_| ())
}

pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
