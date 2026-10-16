//! Iconos de la interfaz. Por defecto usa glifos de Nerd Font; con
//! `PASTOR_ICONS=simple` se queda en símbolos Unicode que muestra cualquier fuente.

use std::sync::OnceLock;
use std::time::Instant;

use crate::agent::{Kind, Status};
use crate::store::TaskStatus;

pub struct Icons {
    pub logo: &'static str,
    pub agents: &'static str,
    pub tasks: &'static str,
    pub dashboard: &'static str,
    pub folder: &'static str,
    pub term: &'static str,
    pub task: &'static str,
    pub bell: &'static str,
    pub done: &'static str,
    kinds: [&'static str; 3],
    starting: &'static str,
    blocked: &'static str,
    idle: &'static str,
    dead: &'static str,
    task_status: [&'static str; 4],
}

const NERD: Icons = Icons {
    logo: "\u{f0cc6}",
    agents: "\u{f06a9}",
    tasks: "\u{f0ae}",
    dashboard: "\u{f056e}",
    folder: "\u{f024b}",
    term: "\u{f120}",
    task: "\u{f0ae}",
    bell: "\u{f009a}",
    done: "\u{f05e0}",
    kinds: ["\u{f09d1}", "\u{f0626}", "\u{f018d}"],
    starting: "\u{f0450}",
    blocked: "\u{f0026}",
    idle: "\u{f04b2}",
    dead: "\u{f0156}",
    task_status: ["\u{f0131}", "\u{f051f}", "\u{f0208}", "\u{f0132}"],
};

const SIMPLE: Icons = Icons {
    logo: "",
    agents: "",
    tasks: "",
    dashboard: "",
    folder: "",
    term: "⊞",
    task: "",
    bell: "",
    done: "",
    kinds: ["", "", ""],
    starting: "◌",
    blocked: "▲",
    idle: "○",
    dead: "✕",
    task_status: ["○", "◐", "◆", "●"],
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// El juego de iconos elegido con `PASTOR_ICONS` (`nerd` por defecto).
pub fn get() -> &'static Icons {
    static ICONS: OnceLock<&'static Icons> = OnceLock::new();
    ICONS.get_or_init(|| match std::env::var("PASTOR_ICONS").as_deref() {
        Ok("simple") => &SIMPLE,
        _ => &NERD,
    })
}

impl Icons {
    pub fn kind(&self, k: Kind) -> &'static str {
        self.kinds[Kind::ALL.iter().position(|x| *x == k).unwrap_or(0)]
    }

    /// Los agentes trabajando muestran un spinner animado.
    pub fn status(&self, st: Status) -> &'static str {
        match st {
            Status::Starting => self.starting,
            Status::Working => {
                static START: OnceLock<Instant> = OnceLock::new();
                let ms = START.get_or_init(Instant::now).elapsed().as_millis();
                SPINNER[(ms / 100) as usize % SPINNER.len()]
            }
            Status::Blocked => self.blocked,
            Status::Idle => self.idle,
            Status::Dead => self.dead,
        }
    }

    pub fn task_status(&self, st: TaskStatus) -> &'static str {
        match st {
            TaskStatus::Todo => self.task_status[0],
            TaskStatus::InProgress => self.task_status[1],
            TaskStatus::Review => self.task_status[2],
            TaskStatus::Done => self.task_status[3],
        }
    }
}

/// `icon` seguido de un espacio, o nada si el juego no tiene ese icono.
pub fn pad(icon: &str) -> String {
    if icon.is_empty() {
        String::new()
    } else {
        format!("{icon} ")
    }
}
