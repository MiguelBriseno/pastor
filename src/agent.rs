//! Tipos de agente, cómo lanzarlos y cómo deducir su estado.

use std::fmt;
use std::path::PathBuf;

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

    pub fn bin(self) -> &'static str {
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
        // Con headroom instalado, Claude Code pasa por su proxy. Tu alias de
        // `claude` no aplica aquí: tmux lanza `sh` directamente.
        let mut cmd = match (self, headroom()) {
            (Kind::Claude, Some(h)) => vec![
                h.to_string_lossy().into_owned(),
                "wrap".into(),
                "claude".into(),
                "--".into(),
            ],
            _ => vec![self.bin().to_string()],
        };
        match self {
            Kind::Claude => {
                // Los hooks nos avisan del estado real de Claude Code.
                cmd.push("--settings".into());
                cmd.push(hooks::claude_settings()?.to_string_lossy().into_owned());
                push_prompt(&mut cmd, prompt);
            }
            Kind::Codex => push_prompt(&mut cmd, prompt),
            Kind::Opencode => {
                // Junto con `=`: si empieza por `-`, que no se tome como opción.
                if let Some(p) = prompt {
                    cmd.push(format!("--prompt={p}"));
                }
            }
        }
        Ok(cmd)
    }
}

/// El prompt va tras `--`: si empieza por `-`, que no se tome como opción.
fn push_prompt(cmd: &mut Vec<String>, prompt: Option<&str>) {
    if let Some(p) = prompt {
        cmd.push("--".into());
        cmd.push(p.into());
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.bin())
    }
}

/// headroom, si está instalado y no se desactivó con `PASTOR_HEADROOM=0`.
pub fn headroom() -> Option<PathBuf> {
    if std::env::var("PASTOR_HEADROOM").as_deref() == Ok("0") {
        return None;
    }
    find_in_path("headroom")
}

fn in_path(bin: &str) -> bool {
    find_in_path(bin).is_some()
}

pub(crate) fn find_in_path(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(bin))
            .find(|f| f.is_file())
    })
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
    /// El agente salió y en su panel quedó el shell (u otro programa).
    pub gone: bool,
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

/// Lo que deja Claude Code al interrumpir un turno (p. ej. al rechazar un
/// permiso con Esc). No lanza `Stop`, así que su último hook puede ser un
/// `blocked` que ya no vale. Solo cuenta al final de la pantalla: un diálogo
/// posterior lo empuja hacia arriba.
const INTERRUPTED: &[&str] = &["what should claude do instead"];

pub fn classify(obs: &Observation, hook: Option<&HookStatus>) -> Status {
    if obs.dead || obs.gone {
        return Status::Dead;
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
    let last = tail_lower(obs.screen, 8);
    let interrupted = INTERRUPTED.iter().any(|p| last.contains(p));
    match hook.map(|h| h.state.as_str()) {
        Some("blocked") if !interrupted => return Status::Blocked,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_prompt_no_se_toma_como_opcion() {
        let mut cmd = vec!["codex".to_string()];
        push_prompt(&mut cmd, Some("-v revisa"));
        assert_eq!(cmd, ["codex", "--", "-v revisa"]);
        let mut cmd = vec!["codex".to_string()];
        push_prompt(&mut cmd, None);
        assert_eq!(cmd, ["codex"]);
    }

    fn obs(screen: &str) -> Observation<'_> {
        Observation {
            dead: false,
            gone: false,
            screen,
            last_change: None,
            age_secs: 60,
        }
    }

    fn hook(state: &str, secs_ago: i64) -> HookStatus {
        HookStatus {
            state: state.into(),
            at: Utc::now() - Duration::seconds(secs_ago),
            message: String::new(),
        }
    }

    #[test]
    fn muerto_o_salido_gana_a_todo() {
        let o = Observation {
            dead: true,
            ..obs("Do you want to proceed?")
        };
        assert_eq!(classify(&o, None), Status::Dead);
        let o = Observation {
            gone: true,
            ..obs("esc to interrupt")
        };
        assert_eq!(classify(&o, Some(&hook("working", 0))), Status::Dead);
    }

    #[test]
    fn textos_de_pantalla() {
        assert_eq!(
            classify(&obs("Do you want to make this edit?\n❯ 1. Yes"), None),
            Status::Blocked
        );
        assert_eq!(
            classify(&obs("✻ Pensando… (esc to interrupt)"), None),
            Status::Working
        );
        // Pedir permiso pesa más que estar trabajando.
        assert_eq!(
            classify(&obs("esc to interrupt\nAllow once"), None),
            Status::Blocked
        );
    }

    #[test]
    fn solo_cuenta_el_final_de_la_pantalla() {
        let mut screen = String::from("Do you want to proceed?\n");
        for i in 0..20 {
            screen.push_str(&format!("línea {i}\n\n"));
        }
        assert_eq!(classify(&obs(&screen), None), Status::Idle);
    }

    #[test]
    fn sin_textos_decide_el_tiempo() {
        let nuevo = Observation {
            age_secs: 2,
            ..obs("")
        };
        assert_eq!(classify(&nuevo, None), Status::Starting);
        assert_eq!(classify(&obs(""), None), Status::Idle);
        let activo = Observation {
            last_change: Some(Utc::now()),
            ..obs("")
        };
        assert_eq!(classify(&activo, None), Status::Working);
        let quieto = Observation {
            last_change: Some(Utc::now() - Duration::seconds(10)),
            ..obs("")
        };
        assert_eq!(classify(&quieto, None), Status::Idle);
    }

    #[test]
    fn hooks() {
        assert_eq!(
            classify(&obs(""), Some(&hook("blocked", 30))),
            Status::Blocked
        );
        let activo = Observation {
            last_change: Some(Utc::now()),
            ..obs("")
        };
        assert_eq!(classify(&activo, Some(&hook("idle", 0))), Status::Idle);
        // "working" sin cambios en pantalla no basta.
        assert_eq!(classify(&obs(""), Some(&hook("working", 30))), Status::Idle);
    }

    #[test]
    fn hook_viejo_no_cuenta_si_la_pantalla_cambio_despues() {
        let o = Observation {
            last_change: Some(Utc::now()),
            ..obs("")
        };
        assert_eq!(classify(&o, Some(&hook("blocked", 10))), Status::Working);
    }

    /// Final de la pantalla de Claude Code tras rechazar un permiso con Esc.
    const INTERRUMPIDO: &str = "❯ Ejecuta con Bash: curl -s https://example.com\n\
        \x20 ⎿  Interrupted · What should Claude do instead?\n\
        ✻ Baked for 8s · done 11:27 a.m.\n\
        ────────\n❯ \n────────\n  ⏸ manual mode on · ? for shortcuts";

    #[test]
    fn rechazar_un_permiso_no_deja_esperando() {
        assert_eq!(
            classify(&obs(INTERRUMPIDO), Some(&hook("blocked", 30))),
            Status::Idle
        );
    }

    #[test]
    fn un_dialogo_posterior_a_la_interrupcion_sigue_esperando() {
        let mut screen = format!("{INTERRUMPIDO}\n❯ y ahora pregúntame algo\n");
        for i in 1..=8 {
            screen.push_str(&format!("opción {i}\n"));
        }
        assert_eq!(
            classify(&obs(&screen), Some(&hook("blocked", 1))),
            Status::Blocked
        );
    }

    #[test]
    fn tail_lower_ignora_lineas_vacias() {
        assert_eq!(tail_lower("A\n\n  \nB\nC\n", 2), "b\nc");
        assert_eq!(tail_lower("Uno", 5), "uno");
        assert_eq!(tail_lower("", 3), "");
    }
}
