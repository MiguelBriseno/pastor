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
/// `refresh` lee lo nuevo en vez del archivo entero, y lo suma a los totales
/// sin guardar las entradas. Las líneas que no se entienden (p. ej. eventos de
/// una versión más nueva) se ignoran.
#[derive(Default)]
pub struct Tail {
    totals: Totals,
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
        for l in String::from_utf8_lossy(&buf[..=end]).lines() {
            if let Ok(e) = serde_json::from_str(l) {
                self.totals.add(&e);
            }
        }
        self.offset += end as u64 + 1;
        true
    }

    pub fn stats(&self) -> Stats {
        self.totals.stats(Local::now().date_naive())
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

/// Lo acumulado de las entradas leídas: lo que hace falta para las
/// estadísticas, que dependen además del día de hoy.
#[derive(Default)]
struct Totals {
    since: Option<DateTime<Utc>>,
    sessions: u64,
    tasks_created: u64,
    messages: u64,
    turns: u64,
    by_kind: [u64; Kind::ALL.len()],
    projects: HashMap<PathBuf, u64>,
    /// Tareas hechas alguna vez: una tarea puede reabrirse y cerrarse otra
    /// vez, y cuenta una sola.
    done: HashSet<String>,
    session_secs: (i64, i64),
    task_secs: (i64, i64),
    /// Eventos por día (hora local).
    daily: HashMap<NaiveDate, u64>,
}

impl Totals {
    fn add(&mut self, e: &Entry) {
        let mut activity = true;
        match &e.event {
            Event::Baseline {
                since,
                agents,
                tasks,
            } => {
                self.since = Some(self.since.map_or(*since, |d| d.min(*since)));
                self.sessions += u64::from(*agents);
                self.tasks_created += u64::from(*tasks);
                activity = false;
            }
            Event::AgentStarted { kind, cwd, .. } => {
                self.sessions += 1;
                if let Some(i) = Kind::ALL.iter().position(|k| k == kind) {
                    self.by_kind[i] += 1;
                }
                *self.projects.entry(cwd.clone()).or_default() += 1;
            }
            Event::AgentEnded { secs, .. } => {
                self.session_secs.0 += secs;
                self.session_secs.1 += 1;
                activity = false;
            }
            Event::TaskCreated { .. } => self.tasks_created += 1,
            Event::TaskStatus { task, to, secs } => {
                if *to == TaskStatus::Done && !self.done.contains(task) {
                    self.done.insert(task.clone());
                    self.task_secs.0 += secs;
                    self.task_secs.1 += 1;
                }
            }
            Event::TaskDeleted { .. } => activity = false,
            Event::MessageSent { .. } => self.messages += 1,
            Event::Turn { .. } => self.turns += 1,
        }
        self.since = Some(self.since.map_or(e.ts, |d| d.min(e.ts)));
        if activity {
            *self
                .daily
                .entry(e.ts.with_timezone(&Local).date_naive())
                .or_default() += 1;
        }
    }

    fn stats(&self, today: NaiveDate) -> Stats {
        let avg = |(total, n): (i64, i64)| (n > 0).then(|| total / n);
        let mut projects: Vec<_> = self.projects.clone().into_iter().collect();
        projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let first_day = today - Duration::days(DAYS as i64 - 1);
        Stats {
            since: self.since,
            sessions: self.sessions,
            tasks_created: self.tasks_created,
            tasks_done: self.done.len() as u64,
            messages: self.messages,
            turns: self.turns,
            avg_session_secs: avg(self.session_secs),
            avg_task_secs: avg(self.task_secs),
            agent_secs: self.session_secs.0,
            by_kind: self.by_kind.to_vec(),
            projects,
            daily: (0..DAYS as i64)
                .map(|i| {
                    let day = first_day + Duration::days(i);
                    (day, self.daily.get(&day).copied().unwrap_or(0))
                })
                .collect(),
        }
    }
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
        assert_eq!(t.stats().turns, 2);

        // Rehecho más corto: se vuelve a leer desde el principio.
        fs::write(&path, turn("a3")).unwrap();
        assert!(t.refresh_from(&path));
        assert_eq!(t.stats().turns, 1);

        // Borrado y vuelto a crear más largo: otro archivo, desde el principio.
        fs::remove_file(&path).unwrap();
        fs::write(&path, [turn("a4"), turn("a5"), turn("a6")].concat()).unwrap();
        assert!(t.refresh_from(&path));
        assert_eq!(t.stats().turns, 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn estadisticas_acumuladas() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        // Al mediodía local, para que el día no dependa de la zona horaria.
        let at = |days_ago: i64| {
            (today - Duration::days(days_ago))
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .unwrap()
                .with_timezone(&Utc)
        };
        let started = |cwd: &str| Event::AgentStarted {
            agent: "a".into(),
            kind: Kind::Codex,
            cwd: cwd.into(),
        };
        let done = |task: &str, secs| Event::TaskStatus {
            task: task.into(),
            to: TaskStatus::Done,
            secs,
        };
        let events = [
            (
                30,
                Event::Baseline {
                    since: at(40),
                    agents: 2,
                    tasks: 3,
                },
            ),
            (20, started("/b")),
            (1, started("/a")),
            (1, started("/a")),
            (
                0,
                Event::AgentEnded {
                    agent: "a".into(),
                    kind: Kind::Codex,
                    secs: 100,
                },
            ),
            (0, done("t1", 60)),
            // Reabierta y cerrada otra vez: no cuenta de nuevo.
            (0, done("t1", 600)),
            (0, done("t2", 120)),
        ];
        let mut t = Totals::default();
        for (days_ago, event) in events {
            t.add(&Entry {
                ts: at(days_ago),
                event,
            });
        }
        let s = t.stats(today);
        assert_eq!(s.since, Some(at(40)));
        assert_eq!((s.sessions, s.tasks_created, s.tasks_done), (5, 3, 2));
        assert_eq!((s.avg_session_secs, s.avg_task_secs), (Some(100), Some(90)));
        let codex = Kind::ALL.iter().position(|k| *k == Kind::Codex).unwrap();
        assert_eq!(s.by_kind[codex], 3);
        assert_eq!(
            s.projects,
            [(PathBuf::from("/a"), 2), (PathBuf::from("/b"), 1)]
        );
        // Lo de hace 20 días queda fuera del gráfico; el fin de una sesión
        // no cuenta como actividad.
        assert_eq!(s.daily.len(), DAYS);
        assert_eq!(s.daily[DAYS - 1], (today, 3));
        assert_eq!(s.daily[DAYS - 2].1, 2);
        assert_eq!(s.daily.iter().map(|d| d.1).sum::<u64>(), 5);
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
