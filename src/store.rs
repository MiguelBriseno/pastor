//! Estado persistente: agentes y tareas en ~/.local/share/pastor/state.json.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::Kind;

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pastor")
}

pub fn ensure_dirs() -> Result<()> {
    fs::create_dir_all(data_dir().join("status"))?;
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub cwd: PathBuf,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub task: Option<String>,
    /// Último comando usado en la terminal secundaria (p. ej. "npm run dev").
    #[serde(default)]
    pub term_cmd: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Review,
    Done,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Todo => "pendiente",
            TaskStatus::InProgress => "en curso",
            TaskStatus::Review => "por revisar",
            TaskStatus::Done => "hecha",
        }
    }

    pub fn next(self) -> Self {
        match self {
            TaskStatus::Todo => TaskStatus::InProgress,
            TaskStatus::InProgress => TaskStatus::Review,
            TaskStatus::Review => TaskStatus::Done,
            TaskStatus::Done => TaskStatus::Todo,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub agent: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Origen externo (p. ej. "asana:<gid>"), para las integraciones.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    next_agent: u32,
    #[serde(default)]
    next_task: u32,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl State {
    pub fn load() -> Result<Self> {
        Self::load_in(&data_dir())
    }

    /// Carga, modifica y guarda. Todas las escrituras pasan por aquí, con un
    /// candado sobre `state.lock`, para que la TUI, la CLI y los hooks de
    /// varios agentes puedan escribir a la vez sin pisarse. Leer no lo
    /// necesita: `rename` cambia el archivo de golpe. `f` no debe llamar a
    /// `update` (se quedaría esperando su propio candado).
    pub fn update<T>(f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        ensure_dirs()?;
        Self::update_in(&data_dir(), f)
    }

    fn load_in(dir: &Path) -> Result<Self> {
        match fs::read_to_string(dir.join("state.json")) {
            Ok(s) => serde_json::from_str(&s).context("state.json está corrupto"),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn update_in<T>(dir: &Path, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let lock_path = dir.join("state.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("no pude abrir {}", lock_path.display()))?;
        match lock.lock() {
            // Algunos montajes (NFS, FUSE) no tienen candados: mejor seguir
            // como antes que dejar de funcionar.
            Err(e) if e.kind() == ErrorKind::Unsupported => {}
            r => r.with_context(|| format!("no pude bloquear {}", lock_path.display()))?,
        }
        let mut s = Self::load_in(dir)?;
        let out = f(&mut s)?;
        // El temporal es compartido: solo se escribe con el candado tomado.
        let path = dir.join("state.json");
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&s)?)?;
        fs::rename(tmp, path)?;
        Ok(out)
    }

    /// Cuántos agentes y tareas se han creado en total.
    pub fn counters(&self) -> (u32, u32) {
        (self.next_agent, self.next_task)
    }

    pub fn add_agent(&mut self, kind: Kind, cwd: PathBuf, name: Option<String>) -> Agent {
        self.next_agent += 1;
        let base = name
            .map(|n| n.split_whitespace().collect::<Vec<_>>().join("-"))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                cwd.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| kind.to_string())
            });
        let mut name = base.clone();
        let mut i = 2;
        while self.agents.iter().any(|a| a.name == name) {
            name = format!("{base}-{i}");
            i += 1;
        }
        let agent = Agent {
            id: format!("a{}", self.next_agent),
            name,
            kind,
            cwd,
            created_at: Utc::now(),
            task: None,
            term_cmd: None,
        };
        self.agents.push(agent.clone());
        agent
    }

    pub fn find_agent(&self, key: &str) -> Result<&Agent> {
        self.agents
            .iter()
            .find(|a| a.id == key || a.name == key)
            .ok_or_else(|| anyhow!("no existe el agente `{key}`"))
    }

    pub fn add_task(&mut self, title: String, description: String) -> Task {
        self.next_task += 1;
        let task = Task {
            id: format!("t{}", self.next_task),
            title,
            description,
            status: TaskStatus::Todo,
            agent: None,
            created_at: Utc::now(),
            source: None,
            url: None,
        };
        self.tasks.push(task.clone());
        task
    }

    pub fn find_task(&self, id: &str) -> Result<&Task> {
        self.tasks
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow!("no existe la tarea `{id}`"))
    }

    pub fn find_task_mut(&mut self, id: &str) -> Result<&mut Task> {
        self.tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow!("no existe la tarea `{id}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nombres_de_agente() {
        let mut s = State::default();
        let dir = PathBuf::from("/home/yo/web");
        assert_eq!(s.add_agent(Kind::Claude, dir.clone(), None).name, "web");
        assert_eq!(s.add_agent(Kind::Codex, dir.clone(), None).name, "web-2");
        assert_eq!(s.add_agent(Kind::Claude, dir.clone(), None).name, "web-3");
        let a = s.add_agent(Kind::Claude, dir.clone(), Some("  arreglar  login ".into()));
        assert_eq!(a.name, "arreglar-login");
        assert_eq!(a.id, "a4");
        let a = s.add_agent(Kind::Opencode, PathBuf::from("/"), Some("   ".into()));
        assert_eq!(a.name, "opencode");
        assert_eq!(s.find_agent("web-2").unwrap().id, "a2");
        assert!(s.find_agent("nadie").is_err());
    }

    #[test]
    fn los_ids_no_se_reutilizan() {
        let mut s = State::default();
        s.add_task("uno".into(), String::new());
        s.tasks.clear();
        assert_eq!(s.add_task("dos".into(), String::new()).id, "t2");
    }

    #[test]
    fn ciclo_de_estados_de_tarea() {
        let mut t = TaskStatus::Todo;
        for esperado in [
            TaskStatus::InProgress,
            TaskStatus::Review,
            TaskStatus::Done,
            TaskStatus::Todo,
        ] {
            t = t.next();
            assert_eq!(t, esperado);
        }
    }

    /// Muchos escritores a la vez (como los hooks de varios agentes) no
    /// pierden cambios ni corrompen el archivo.
    #[test]
    fn escrituras_simultaneas() {
        let dir = std::env::temp_dir().join(format!(
            "pastor-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let (hilos, por_hilo) = (8, 25);
        let handles: Vec<_> = (0..hilos)
            .map(|h| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..por_hilo {
                        State::update_in(&dir, |s| {
                            Ok(s.add_task(format!("{h}-{i}"), String::new()))
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        // Esperar a todos antes de borrar, aunque alguno falle.
        let joined: Vec<_> = handles.into_iter().map(|h| h.join()).collect();
        let ok = joined.iter().all(|r| r.is_ok());
        let s = State::load_in(&dir);
        let _ = fs::remove_dir_all(&dir);
        assert!(ok);
        let s = s.unwrap();
        assert_eq!(s.tasks.len(), hilos * por_hilo);
        assert_eq!(s.counters().1 as usize, hilos * por_hilo);
    }
}
