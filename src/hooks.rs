//! Hooks de Claude Code: `pastor hook <evento>` escribe el estado del agente
//! en ~/.local/share/pastor/status/<id>.json.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent;
use crate::history::{self, Event};
use crate::ops;
use crate::store::{data_dir, ensure_dirs};
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
        "notify" => {
            let idle = v["notification_type"] == "idle_prompt"
                || message.contains("waiting for your input");
            if idle {
                "idle"
            } else {
                "blocked"
            }
        }
        "idle" => "idle",
        _ => "working",
    };
    ensure_dirs()?;
    let status = HookStatus {
        state: state.into(),
        at: Utc::now(),
        message,
    };
    fs::write(status_path(&id), serde_json::to_string(&status)?)?;

    // Claude terminó su turno: su tarea queda lista para revisar.
    if event == "idle" {
        history::record(Event::Turn { agent: id.clone() });
        ops::finish_turn(&id)?;
    }
    Ok(())
}

/// Escribe el settings.json que se pasa a `claude --settings`.
/// Se suma a tu configuración normal de Claude Code, no la reemplaza.
pub fn claude_settings() -> Result<PathBuf> {
    let exe = shell_quote(&std::env::current_exe()?.to_string_lossy());
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
    fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
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

    let q = |p: &PathBuf| shell_quote(&p.to_string_lossy());
    let prepend = format!("PATH={}:\"$PATH\"\n", q(&bin));
    fs::write(
        dir.join("bashrc"),
        format!("[ -f ~/.bashrc ] && . ~/.bashrc\n{prepend}"),
    )?;
    let zdot = dir.join("zsh");
    fs::create_dir_all(&zdot)?;
    fs::write(
        zdot.join(".zshenv"),
        "[ -f \"${PASTOR_ZDOTDIR:-$HOME}/.zshenv\" ] && . \"${PASTOR_ZDOTDIR:-$HOME}/.zshenv\"\n",
    )?;
    fs::write(
        zdot.join(".zshrc"),
        format!(
            "ZDOTDIR=\"${{PASTOR_ZDOTDIR:-$HOME}}\"\nunset PASTOR_ZDOTDIR\n\
             [ -f \"$ZDOTDIR/.zshrc\" ] && . \"$ZDOTDIR/.zshrc\"\n{prepend}"
        ),
    )?;

    Ok(format!(
        r#"sh_="${{SHELL:-/bin/sh}}"
case "${{sh_##*/}}" in
  bash) exec "$sh_" --rcfile {bashrc} -i ;;
  zsh) PASTOR_ZDOTDIR="${{ZDOTDIR:-$HOME}}" ZDOTDIR={zdot} exec "$sh_" -i ;;
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
    let script = format!(
        "#!/bin/sh\n{via_headroom}exec {} --settings {} \"$@\"\n",
        shell_quote(&real.to_string_lossy()),
        shell_quote(&claude_settings()?.to_string_lossy()),
    );
    let path = bin.join("claude");
    fs::write(&path, script)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
