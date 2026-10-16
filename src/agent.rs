//! Tipos de agente, cómo lanzarlos y cómo deducir su estado.

use std::fmt;

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::hooks::{self, HookStatus};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Claude,
    Codex,
    Opencode,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Claude, Kind::Codex, Kind::Opencode];

    fn bin(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Opencode => "opencode",
        }
    }

    pub fn launch_command(self, prompt: Option<&str>) -> Result<Vec<String>> {
        if !in_path(self.bin()) {
            bail!("no encuentro `{}` en el PATH", self.bin());
        }
        let mut cmd = vec![self.bin().to_string()];
        match self {
            Kind::Claude => {
                // Los hooks nos avisan del estado real de Claude Code.
                cmd.push("--settings".into());
                cmd.push(hooks::claude_settings()?.to_string_lossy().into_owned());
                cmd.extend(prompt.map(String::from));
            }
            Kind::Codex => cmd.extend(prompt.map(String::from)),
            Kind::Opencode => {
                if let Some(p) = prompt {
                    cmd.push("--prompt".into());
                    cmd.push(p.into());
                }
            }
        }
        Ok(cmd)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.bin())
    }
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    Working,
    Blocked,
    Idle,
    Dead,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Starting => "iniciando",
            Status::Working => "trabajando",
            Status::Blocked => "esperando",
            Status::Idle => "inactivo",
            Status::Dead => "terminado",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Status::Starting => "◌",
            Status::Working => "●",
            Status::Blocked => "▲",
            Status::Idle => "○",
            Status::Dead => "✕",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Status::Starting => Color::Cyan,
            Status::Working => Color::Green,
            Status::Blocked => Color::Yellow,
            Status::Idle => Color::Blue,
            Status::Dead => Color::DarkGray,
        }
    }
}

pub struct Observation<'a> {
    pub dead: bool,
    /// El agente salió y en el panel quedó el shell.
    pub at_shell: bool,
    pub screen: &'a str,
    /// Última vez que vimos cambiar la pantalla (None = no ha cambiado desde que miramos).
    pub last_change: Option<DateTime<Utc>>,
    pub age_secs: i64,
}

/// Textos que indican que el agente espera una decisión tuya.
const BLOCKED: &[&str] = &[
    "do you want to",
    "would you like to",
    "do you trust",
    "trust this folder",
    "allow once",
    "allow always",
    "permission required",
    "(y/n)",
    "[y/n]",
    "enter to confirm",
    "yes, proceed",
];

/// Textos que muestran los tres agentes mientras trabajan.
const WORKING: &[&str] = &["esc to interrupt", "esc interrupt"];

pub fn classify(obs: &Observation, hook: Option<&HookStatus>) -> Status {
    if obs.dead {
        return Status::Dead;
    }
    // Al arrancar, `sh` está en primer plano un instante antes que el agente.
    if obs.at_shell {
        return if obs.age_secs < 5 {
            Status::Starting
        } else {
            Status::Dead
        };
    }
    let tail = tail_lower(obs.screen, 15);
    if BLOCKED.iter().any(|p| tail.contains(p)) {
        return Status::Blocked;
    }
    if WORKING.iter().any(|p| tail.contains(p)) {
        return Status::Working;
    }
    let now = Utc::now();
    let changed_recently = obs
        .last_change
        .is_some_and(|t| now - t < Duration::seconds(3));
    // Un hook vale mientras la pantalla no haya cambiado después de él.
    let hook = hook.filter(|h| {
        obs.last_change
            .is_none_or(|c| h.at + Duration::seconds(2) >= c)
    });
    match hook.map(|h| h.state.as_str()) {
        Some("blocked") => return Status::Blocked,
        Some("idle") => return Status::Idle,
        Some("working") if changed_recently => return Status::Working,
        _ => {}
    }
    match (changed_recently, obs.age_secs < 5) {
        (_, true) => Status::Starting,
        (true, false) => Status::Working,
        (false, false) => Status::Idle,
    }
}

fn tail_lower(screen: &str, n: usize) -> String {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..]
        .join("\n")
        .to_lowercase()
}
