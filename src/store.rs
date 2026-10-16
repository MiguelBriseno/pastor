//! Estado persistente: agentes y tareas en ~/.local/share/pastor/state.json.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

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
    fn path() -> PathBuf {
        data_dir().join("state.json")
    }

    pub fn load() -> Result<Self> {
        match fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).context("state.json está corrupto"),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        ensure_dirs()?;
        let tmp = Self::path().with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(tmp, Self::path())?;
        Ok(())
    }

    /// Carga, modifica y guarda. Todas las escrituras pasan por aquí para
    /// que la TUI y la CLI puedan usarse a la vez sin pisarse.
    pub fn update<T>(f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut s = Self::load()?;
        let out = f(&mut s)?;
        s.save()?;
        Ok(out)
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
