//! Operaciones compartidas entre la CLI y la TUI.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::agent::{self, Kind, Observation, Status};
use crate::hooks;
use crate::store::{Agent, State, TaskStatus};
use crate::tmux;

pub fn new_agent(
    kind: Kind,
    cwd: Option<PathBuf>,
    name: Option<String>,
    prompt: Option<&str>,
) -> Result<Agent> {
    tmux::ensure_available()?;
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("el directorio {} no existe", cwd.display()))?;
    let cmd = kind.launch_command(prompt)?;
    let agent = State::update(|s| Ok(s.add_agent(kind, cwd, name)))?;
    hooks::clear_status(&agent.id);
    if let Err(e) = tmux::new_session(&agent.id, &agent.cwd, &[("PASTOR_AGENT", &agent.id)], &cmd) {
        let _ = State::update(|s| {
            s.agents.retain(|a| a.id != agent.id);
            Ok(())
        });
        return Err(e);
    }
    Ok(agent)
}

pub fn send(agent_key: &str, text: &str) -> Result<()> {
    let state = State::load()?;
    let agent = state.find_agent(agent_key)?;
    tmux::send_text(&agent.id, text)
}

pub fn kill_agent(agent_key: &str) -> Result<()> {
    let id = State::load()?.find_agent(agent_key)?.id.clone();
    let _ = tmux::kill_session(&id);
    hooks::clear_status(&id);
    State::update(|s| {
        s.agents.retain(|a| a.id != id);
        for t in s
            .tasks
            .iter_mut()
            .filter(|t| t.agent.as_deref() == Some(&id))
        {
            t.agent = None;
            if t.status == TaskStatus::InProgress {
                t.status = TaskStatus::Todo;
            }
        }
        Ok(())
    })
}

pub enum AssignTarget {
    Existing(String),
    New { kind: Kind, cwd: PathBuf },
}

/// Envía la tarea a un agente (existente o nuevo) y la marca "en curso".
pub fn assign_task(task_id: &str, target: AssignTarget) -> Result<Agent> {
    let state = State::load()?;
    let task = state.find_task(task_id)?.clone();
    let mut prompt = format!("[{}] {}", task.id, task.title);
    if !task.description.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&task.description);
    }
    let agent = match target {
        AssignTarget::Existing(key) => {
            let a = state.find_agent(&key)?.clone();
            tmux::send_text(&a.id, &prompt)?;
            a
        }
        AssignTarget::New { kind, cwd } => new_agent(kind, Some(cwd), None, Some(&prompt))?,
    };
    State::update(|s| {
        let t = s.find_task_mut(&task.id)?;
        t.agent = Some(agent.id.clone());
        t.status = TaskStatus::InProgress;
        if let Some(a) = s.agents.iter_mut().find(|a| a.id == agent.id) {
            a.task = Some(task.id.clone());
        }
        Ok(())
    })?;
    Ok(agent)
}

pub fn set_task_status(task_id: &str, status: TaskStatus) -> Result<()> {
    State::update(|s| {
        s.find_task_mut(task_id)?.status = status;
        Ok(())
    })
}

pub fn delete_task(task_id: &str) -> Result<()> {
    State::update(|s| {
        s.find_task(task_id)?;
        s.tasks.retain(|t| t.id != task_id);
        for a in s
            .agents
            .iter_mut()
            .filter(|a| a.task.as_deref() == Some(task_id))
        {
            a.task = None;
        }
        Ok(())
    })
}

/// Recuerda cuándo cambió por última vez la pantalla de cada agente.
#[derive(Default)]
pub struct Tracker {
    seen: HashMap<String, (u64, Option<DateTime<Utc>>)>,
}

impl Tracker {
    fn observe(&mut self, id: &str, screen: &str) -> Option<DateTime<Utc>> {
        let mut h = DefaultHasher::new();
        screen.hash(&mut h);
        let h = h.finish();
        match self.seen.get_mut(id) {
            Some(entry) => {
                if entry.0 != h {
                    *entry = (h, Some(Utc::now()));
                }
                entry.1
            }
            None => {
                self.seen.insert(id.to_string(), (h, None));
                None
            }
        }
    }
}

pub struct Snapshot {
    pub status: Status,
    pub screen: String,
}

pub fn observe(state: &State, tracker: &mut Tracker) -> HashMap<String, Snapshot> {
    let panes = tmux::panes();
    state
        .agents
        .iter()
        .map(|a| {
            let pane = panes.get(&a.id);
            let screen = match pane {
                Some(_) => tmux::capture(&a.id).unwrap_or_default(),
                None => String::new(),
            };
            let obs = Observation {
                dead: pane.is_none_or(|p| p.dead),
                screen: &screen,
                last_change: tracker.observe(&a.id, &screen),
                age_secs: (Utc::now() - a.created_at).num_seconds(),
            };
            let status = agent::classify(&obs, hooks::read_status(&a.id).as_ref());
            (a.id.clone(), Snapshot { status, screen })
        })
        .collect()
}
