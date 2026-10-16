//! Historial de uso en ~/.local/share/pastor/history.jsonl: una línea JSON por
//! evento, siempre agregada al final. Sirve para las estadísticas de la
//! pestaña Historial, que cuentan desde que se instaló pastor.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::Kind;
use crate::store::{data_dir, ensure_dirs, State, TaskStatus};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Primera línea del archivo: lo que ya había antes de llevar historial.
    Baseline {
        since: DateTime<Utc>,
        agents: u32,
        tasks: u32,
    },
    AgentStarted {
        agent: String,
        kind: Kind,
        cwd: PathBuf,
    },
    AgentEnded {
        agent: String,
        kind: Kind,
        secs: i64,
    },
    TaskCreated {
        task: String,
    },
    /// Cambio de estado de una tarea; `secs` es su edad en ese momento.
    TaskStatus {
        task: String,
        to: TaskStatus,
        secs: i64,
    },
    TaskDeleted {
        task: String,
    },
    MessageSent {
        agent: String,
    },
    /// Claude Code terminó un turno (hook `Stop`).
    Turn {
        agent: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub event: Event,
}

pub fn path() -> PathBuf {
    data_dir().join("history.jsonl")
}

/// Crea el historial si no existe, con una línea base que cuenta los agentes
/// y tareas creados antes (los contadores de `state.json` nunca bajan).
/// Se llama al arrancar, antes de cualquier cambio de estado.
pub fn ensure_baseline() -> Result<()> {
    if path().exists() {
        return Ok(());
    }
    let state = State::load()?;
    let (agents, tasks) = state.counters();
    // La carpeta de datos se creó al usar pastor por primera vez.
    let dir_created = fs::metadata(data_dir())
        .and_then(|m| m.created())
        .ok()
        .map(DateTime::<Utc>::from);
    let since = state
        .agents
        .iter()
        .map(|a| a.created_at)
        .chain(state.tasks.iter().map(|t| t.created_at))
        .chain(dir_created)
        .min()
        .unwrap_or_else(Utc::now);
    write(&Event::Baseline {
        since,
        agents,
        tasks,
    })
}

/// Anota un evento. El historial es un extra: si falla, no interrumpe nada.
pub fn record(event: Event) {
    let _ = write(&event);
}

fn write(event: &Event) -> Result<()> {
    ensure_dirs()?;
    let entry = Entry {
        ts: Utc::now(),
        event: event.clone(),
    };
    let mut line = serde_json::to_string(&entry)?;
    line.push('\n');
    // Una sola escritura en modo append: la CLI, la TUI y los hooks pueden
    // anotar a la vez sin mezclar líneas.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())?
        .write_all(line.as_bytes())?;
    Ok(())
}

/// Lee el historial. Las líneas que no se entienden (p. ej. eventos de una
/// versión más nueva) se ignoran.
pub fn load() -> Vec<Entry> {
    fs::read_to_string(path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Días que muestra el gráfico de actividad.
pub const DAYS: usize = 14;

#[derive(Default)]
pub struct Stats {
    pub since: Option<DateTime<Utc>>,
    pub sessions: u64,
    pub tasks_created: u64,
    pub tasks_done: u64,
    pub messages: u64,
    pub turns: u64,
    pub avg_session_secs: Option<i64>,
    pub avg_task_secs: Option<i64>,
    /// Tiempo total que han vivido los agentes terminados.
    pub agent_secs: i64,
    /// Sesiones por tipo, en el orden de `Kind::ALL`.
    pub by_kind: Vec<u64>,
    /// Proyectos con más sesiones, de mayor a menor.
    pub projects: Vec<(PathBuf, u64)>,
    /// Eventos por día, del más antiguo a hoy (hora local).
    pub daily: Vec<(NaiveDate, u64)>,
}

pub fn stats(entries: &[Entry]) -> Stats {
    let mut s = Stats {
        by_kind: vec![0; Kind::ALL.len()],
        ..Stats::default()
    };
    let today = Local::now().date_naive();
    let first_day = today - Duration::days(DAYS as i64 - 1);
    let mut daily = vec![0u64; DAYS];
    let mut projects: HashMap<PathBuf, u64> = HashMap::new();
    let mut done: HashSet<&str> = HashSet::new();
    let (mut session_total, mut session_n) = (0i64, 0i64);
    let (mut task_total, mut task_n) = (0i64, 0i64);

    for e in entries {
        let day = e.ts.with_timezone(&Local).date_naive();
        let mut activity = true;
        match &e.event {
            Event::Baseline {
                since,
                agents,
                tasks,
            } => {
                s.since = Some(s.since.map_or(*since, |d| d.min(*since)));
                s.sessions += u64::from(*agents);
                s.tasks_created += u64::from(*tasks);
                activity = false;
            }
            Event::AgentStarted { kind, cwd, .. } => {
                s.sessions += 1;
                if let Some(i) = Kind::ALL.iter().position(|k| k == kind) {
                    s.by_kind[i] += 1;
                }
                *projects.entry(cwd.clone()).or_default() += 1;
            }
            Event::AgentEnded { secs, .. } => {
                session_total += secs;
                session_n += 1;
                activity = false;
            }
            Event::TaskCreated { .. } => s.tasks_created += 1,
            Event::TaskStatus { task, to, secs } => {
                // Una tarea puede reabrirse y cerrarse otra vez: cuenta una vez.
                if *to == TaskStatus::Done && done.insert(task) {
                    s.tasks_done += 1;
                    task_total += secs;
                    task_n += 1;
                }
            }
            Event::TaskDeleted { .. } => activity = false,
            Event::MessageSent { .. } => s.messages += 1,
            Event::Turn { .. } => s.turns += 1,
        }
        s.since = Some(s.since.map_or(e.ts, |d| d.min(e.ts)));
        if activity && day >= first_day && day <= today {
            daily[(day - first_day).num_days() as usize] += 1;
        }
    }

    s.agent_secs = session_total;
    s.avg_session_secs = (session_n > 0).then(|| session_total / session_n);
    s.avg_task_secs = (task_n > 0).then(|| task_total / task_n);
    let mut projects: Vec<_> = projects.into_iter().collect();
    projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    s.projects = projects;
    s.daily = daily
        .into_iter()
        .enumerate()
        .map(|(i, n)| (first_day + Duration::days(i as i64), n))
        .collect();
    s
}

/// Duración corta y legible: "45s", "12m", "3h 20m", "2d 5h".
pub fn human(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match () {
        _ if d > 0 => format!("{d}d {h}h"),
        _ if h > 0 => format!("{h}h {m}m"),
        _ if m > 0 => format!("{m}m"),
        _ => format!("{secs}s"),
    }
}
