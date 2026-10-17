//! Historial de uso en ~/.local/share/pastor/history.jsonl: una línea JSON por
//! evento, siempre agregada al final. Sirve para las estadísticas de la
//! pestaña Historial, que cuentan desde que se instaló pastor.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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
    ensure_dirs()?;
    create_with(
        &path(),
        &line(&Event::Baseline {
            since,
            agents,
            tasks,
        })?,
    )?;
    Ok(())
}

/// Crea `path` con `line` solo si no existe. Si dos procesos llegan a la vez
/// (dos hooks tras actualizar), solo uno escribe la línea base.
fn create_with(path: &Path, line: &str) -> Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(line.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn line(event: &Event) -> Result<String> {
    let entry = Entry {
        ts: Utc::now(),
        event: event.clone(),
    };
    let mut line = serde_json::to_string(&entry)?;
    line.push('\n');
    Ok(line)
}

/// Anota un evento. El historial es un extra: si falla, no interrumpe nada.
pub fn record(event: Event) {
    let _ = write(&event);
}

fn write(event: &Event) -> Result<()> {
    ensure_dirs()?;
    let line = line(event)?;
    // Una sola escritura en modo append: la CLI, la TUI y los hooks pueden
    // anotar a la vez sin mezclar líneas.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())?
        .write_all(line.as_bytes())?;
    Ok(())
}

/// El historial leído hasta ahora. Como solo se le añaden líneas, cada
/// `refresh` lee lo nuevo en vez del archivo entero. Las líneas que no se
/// entienden (p. ej. eventos de una versión más nueva) se ignoran.
#[derive(Default)]
pub struct Tail {
    entries: Vec<Entry>,
    /// Bytes ya leídos, hasta el último salto de línea.
    offset: u64,
    /// Inode del archivo leído, para notar que lo borraron y se creó otro.
    ino: u64,
}

impl Tail {
    /// Lee lo añadido desde la última vez. `true` si cambió algo.
    pub fn refresh(&mut self) -> bool {
        self.refresh_from(&path())
    }

    fn refresh_from(&mut self, path: &Path) -> bool {
        let (len, ino) = fs::metadata(path).map_or((0, 0), |m| (m.len(), m.ino()));
        // Más corto que lo leído, u otro archivo: lo borraron o lo rehicieron.
        let reset = len < self.offset || (self.offset > 0 && ino != self.ino);
        if reset {
            *self = Tail::default();
        }
        self.ino = ino;
        if len == self.offset {
            return reset;
        }
        let mut buf = Vec::new();
        let read = fs::File::open(path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.read_to_end(&mut buf)
        });
        // Una línea a medio escribir se deja para la próxima.
        let Some(end) = read.ok().and(buf.iter().rposition(|b| *b == b'\n')) else {
            return reset;
        };
        self.entries.extend(
            String::from_utf8_lossy(&buf[..=end])
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok()),
        );
        self.offset += end as u64 + 1;
        true
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lee_solo_lo_nuevo_y_espera_lineas_completas() {
        let dir = std::env::temp_dir().join(format!("pastor-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let turn = |a: &str| line(&Event::Turn { agent: a.into() }).unwrap();
        let mut t = Tail::default();
        assert!(!t.refresh_from(&path));

        fs::write(&path, turn("a1")).unwrap();
        assert!(t.refresh_from(&path));
        assert!(!t.refresh_from(&path));

        // Media línea: todavía no cuenta.
        let second = turn("a2");
        let (half, rest) = second.split_at(10);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(half.as_bytes())
            .unwrap();
        assert!(!t.refresh_from(&path));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(rest.as_bytes())
            .unwrap();
        assert!(t.refresh_from(&path));
        assert_eq!(t.entries().len(), 2);

        // Rehecho más corto: se vuelve a leer desde el principio.
        fs::write(&path, turn("a3")).unwrap();
        assert!(t.refresh_from(&path));
        assert_eq!(t.entries().len(), 1);

        // Borrado y vuelto a crear más largo: otro archivo, desde el principio.
        fs::remove_file(&path).unwrap();
        fs::write(&path, [turn("a4"), turn("a5"), turn("a6")].concat()).unwrap();
        assert!(t.refresh_from(&path));
        assert_eq!(t.entries().len(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn la_linea_base_se_escribe_una_sola_vez() {
        let dir = std::env::temp_dir().join(format!("pastor-historial-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let _ = fs::remove_file(&path);
        let created: Vec<bool> = std::thread::scope(|s| {
            let hilos: Vec<_> = (0..8)
                .map(|i| {
                    let path = &path;
                    s.spawn(move || create_with(path, &format!("{i}\n")).unwrap())
                })
                .collect();
            hilos.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let lines = fs::read_to_string(&path).unwrap().lines().count();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(created.iter().filter(|c| **c).count(), 1);
        assert_eq!(lines, 1);
    }
}
