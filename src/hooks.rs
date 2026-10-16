//! Hooks de Claude Code: `pastor hook <evento>` escribe el estado del agente
//! en ~/.local/share/pastor/status/<id>.json.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::history::{self, Event};
use crate::ops;
use crate::store::{data_dir, ensure_dirs, State, TaskStatus};
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
        State::update(|s| {
            let task = s
                .agents
                .iter()
                .find(|a| a.id == id)
                .and_then(|a| a.task.clone());
            if let Some(t) = task.and_then(|t| s.tasks.iter_mut().find(|x| x.id == t)) {
                if t.status == TaskStatus::InProgress {
                    ops::record_status(t, TaskStatus::Review);
                    t.status = TaskStatus::Review;
                }
            }
            Ok(())
        })?;
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
