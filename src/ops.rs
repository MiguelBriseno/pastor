//! Operaciones compartidas entre la CLI y la TUI.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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

/// Los paneles de un agente: el del agente y, si la abrió, su terminal.
#[derive(Default)]
pub struct AgentPanes {
    pub agent: Option<tmux::Pane>,
    pub term: Option<tmux::Pane>,
}

fn split_panes(panes: Vec<tmux::Pane>) -> AgentPanes {
    let mut out = AgentPanes::default();
    for p in panes {
        match p.role.as_str() {
            "term" => out.term = Some(p),
            // Agentes sin marca (versiones anteriores): el primer panel es el agente.
            "agent" | "" if out.agent.is_none() => out.agent = Some(p),
            _ => {}
        }
    }
    out
}

pub fn agent_panes(agent_id: &str) -> AgentPanes {
    split_panes(tmux::panes().remove(agent_id).unwrap_or_default())
}

/// Panel al que hay que hablarle para llegar al agente.
fn agent_target(agent_id: &str) -> String {
    agent_panes(agent_id)
        .agent
        .map_or(agent_id.to_string(), |p| p.id)
}

pub fn send(agent_key: &str, text: &str) -> Result<()> {
    let state = State::load()?;
    let agent = state.find_agent(agent_key)?;
    tmux::send_text(&agent_target(&agent.id), text)
}

/// Entra a la sesión del agente con el foco en el agente.
pub fn attach(agent_key: &str) -> Result<()> {
    let id = State::load()?.find_agent(agent_key)?.id.clone();
    let focus = agent_panes(&id).agent.map(|p| p.id);
    tmux::attach(&id, focus.as_deref())
}

/// Abre una terminal a la derecha del agente, en su mismo directorio, y
/// opcionalmente ejecuta `cmd` en ella (p. ej. "npm run dev").
pub fn open_term(agent_key: &str, cmd: Option<&str>) -> Result<()> {
    let agent = State::load()?.find_agent(agent_key)?.clone();
    let panes = agent_panes(&agent.id);
    let Some(agent_pane) = panes.agent else {
        bail!("el agente {} no está corriendo", agent.name);
    };
    match panes.term {
        Some(t) if !t.dead => bail!("{} ya tiene una terminal abierta", agent.name),
        Some(t) => tmux::kill_pane(&t.id)?,
        None => {}
    }
    let term = tmux::split_right(&agent_pane.id, &agent.cwd)?;
    let cmd = cmd.map(str::trim).filter(|c| !c.is_empty());
    if let Some(c) = cmd {
        tmux::type_line(&term, c)?;
    }
    State::update(|s| {
        if let Some(a) = s.agents.iter_mut().find(|a| a.id == agent.id) {
            if cmd.is_some() {
                a.term_cmd = cmd.map(String::from);
            }
        }
        Ok(())
    })
}

pub fn close_term(agent_key: &str) -> Result<()> {
    let agent = State::load()?.find_agent(agent_key)?.clone();
    match agent_panes(&agent.id).term {
        Some(t) => tmux::kill_pane(&t.id),
        None => bail!("{} no tiene una terminal abierta", agent.name),
    }
}

/// Comando sugerido para la terminal: el último usado o, en proyectos de
/// Node con script "dev", el de su gestor de paquetes.
pub fn suggest_term_cmd(agent: &Agent) -> String {
    if let Some(c) = &agent.term_cmd {
        return c.clone();
    }
    let dir = &agent.cwd;
    let has_dev = std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["scripts"]["dev"].is_string());
    if !has_dev {
        return String::new();
    }
    let exists = |f: &str| Path::exists(&dir.join(f));
    let pm = if exists("pnpm-lock.yaml") {
        "pnpm"
    } else if exists("yarn.lock") {
        "yarn"
    } else if exists("bun.lockb") || exists("bun.lock") {
        "bun"
    } else {
        "npm run"
    };
    format!("{pm} dev")
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
            tmux::send_text(&agent_target(&a.id), &prompt)?;
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

struct Seen {
    hash: u64,
    size: String,
    last_change: Option<DateTime<Utc>>,
}

/// Recuerda cuándo cambió por última vez la pantalla de cada agente.
#[derive(Default)]
pub struct Tracker {
    seen: HashMap<String, Seen>,
}

impl Tracker {
    fn observe(&mut self, id: &str, screen: &str, size: &str) -> Option<DateTime<Utc>> {
        let mut h = DefaultHasher::new();
        screen.hash(&mut h);
        let hash = h.finish();
        match self.seen.get_mut(id) {
            // Si el panel cambió de tamaño (p. ej. al abrir la terminal), el
            // agente se redibuja: eso no cuenta como actividad.
            Some(s) if s.size != size => {
                s.hash = hash;
                s.size = size.to_string();
            }
            Some(s) if s.hash != hash => {
                s.hash = hash;
                s.last_change = Some(Utc::now());
            }
            Some(_) => {}
            None => {
                self.seen.insert(
                    id.to_string(),
                    Seen {
                        hash,
                        size: size.to_string(),
                        last_change: None,
                    },
                );
            }
        }
        self.seen[id].last_change
    }
}

pub struct Snapshot {
    pub status: Status,
    pub screen: String,
    /// Pantalla de la terminal secundaria, si está abierta.
    pub term: Option<TermSnapshot>,
}

pub struct TermSnapshot {
    pub screen: String,
    pub dead: bool,
}

pub fn observe(state: &State, tracker: &mut Tracker) -> HashMap<String, Snapshot> {
    let mut all = tmux::panes();
    state
        .agents
        .iter()
        .map(|a| {
            let panes = split_panes(all.remove(&a.id).unwrap_or_default());
            let pane = panes.agent.as_ref();
            let screen = match pane {
                Some(p) => tmux::capture(&p.id).unwrap_or_default(),
                None => String::new(),
            };
            let term = panes.term.map(|t| TermSnapshot {
                screen: tmux::capture(&t.id).unwrap_or_default(),
                dead: t.dead,
            });
            let obs = Observation {
                dead: pane.is_none_or(|p| p.dead),
                screen: &screen,
                last_change: tracker.observe(&a.id, &screen, pane.map_or("", |p| p.size.as_str())),
                age_secs: (Utc::now() - a.created_at).num_seconds(),
            };
            let status = agent::classify(&obs, hooks::read_status(&a.id).as_ref());
            (
                a.id.clone(),
                Snapshot {
                    status,
                    screen,
                    term,
                },
            )
        })
        .collect()
}
