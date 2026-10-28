//! Operaciones compartidas entre la CLI y la TUI.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};

use crate::agent::{self, Kind, Observation, Status};
use crate::history::{self, Event};
use crate::hooks;
use crate::remote;
use crate::store::{self, Agent, State, Task, TaskStatus, Worktree};
use crate::tmux;
use crate::usage;
use crate::worktree;

/// Dónde trabaja un agente nuevo.
pub enum Place {
    /// En el directorio que se le da.
    Dir,
    /// En un worktree nuevo del repositorio del directorio. Su rama lleva
    /// este nombre o, sin él, el del agente o su tipo.
    NewWorktree(Option<String>),
    /// En un worktree que ya existe: el de una tarea que se vuelve a asignar.
    Worktree(Worktree),
}

pub fn new_agent(
    kind: Kind,
    cwd: Option<PathBuf>,
    name: Option<String>,
    prompt: Option<&str>,
    place: Place,
) -> Result<Agent> {
    tmux::ensure_available()?;
    if let Some(n) = name.as_deref().map(str::trim) {
        if store::looks_like_id(n) {
            bail!("`{n}` parece el id de un agente; elige otro nombre");
        }
    }
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("el directorio {} no existe", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("{} no es un directorio", cwd.display());
    }
    let cmd = kind.launch_command(prompt)?;
    let (cwd, wt, created) = match place {
        Place::Dir => (cwd, None, false),
        Place::NewWorktree(branch) => {
            let branch = branch
                .or_else(|| name.clone())
                .unwrap_or_else(|| kind.to_string());
            let (wt, start) = worktree::create(&cwd, &branch)?;
            (start, Some(wt), true)
        }
        Place::Worktree(wt) => {
            if !wt.path.is_dir() {
                bail!("el worktree {} ya no existe", wt.path.display());
            }
            (wt.path.clone(), Some(wt), false)
        }
    };
    // En un worktree el nombre por defecto es el del repositorio, no el de
    // la copia.
    let name = name.or_else(|| {
        let wt = wt.as_ref()?;
        Some(wt.repo.file_name()?.to_string_lossy().into_owned())
    });
    let launched = launch(kind, cwd, name, &cmd, wt.clone());
    if launched.is_err() && created {
        if let Some(wt) = &wt {
            worktree::remove_if_unchanged(wt);
        }
    }
    launched
}

fn launch(
    kind: Kind,
    cwd: PathBuf,
    name: Option<String>,
    cmd: &[String],
    wt: Option<Worktree>,
) -> Result<Agent> {
    // Si se perdió el estado, el contador vuelve a empezar y `a1` podría
    // seguir vivo: el nuevo chocaría con su sesión y recibiría sus hooks.
    // Sin saber qué sesiones hay, mejor no lanzar: la limpieza de un fallo
    // («duplicate session») mataría la de otro.
    let taken = tmux::panes()?;
    // Fuera del candado: es un proceso de git. En un worktree manda su
    // `repo`, y `cwd` daría la copia.
    let repo = match wt {
        None => worktree::toplevel(&cwd),
        Some(_) => None,
    };
    let agent = State::update(|s| {
        s.skip_ids(|id| taken.contains_key(id));
        s.add_agent(kind, cwd, name);
        let agent = s.agents.last_mut().expect("recién añadido");
        agent.worktree = wt;
        agent.repo = repo;
        Ok(agent.clone())
    })?;
    if let Err(e) = start_session(&agent, cmd) {
        // Que no quede un agente huérfano.
        let _ = State::update(|s| {
            s.agents.retain(|a| a.id != agent.id);
            Ok(())
        });
        return Err(e);
    }
    history::record(Event::AgentStarted {
        agent: agent.id.clone(),
        kind: agent.kind,
        cwd: agent.cwd.clone(),
    });
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
    split_panes(
        tmux::panes()
            .unwrap_or_default()
            .remove(agent_id)
            .unwrap_or_default(),
    )
}

/// Si el agente ya no corre en su panel: salió (con /exit, por ejemplo) y
/// quedó el shell, o lo que el usuario haya abierto ahí. Se mira el proceso en
/// primer plano. Mientras corre el primer agente es el `sh` que lo lanzó, porque
/// comparte grupo de procesos con él, hasta que ese `sh` marca la salida. Lo que
/// se abre después desde el shell interactivo sí queda en primer plano.
fn agent_gone(pane: &tmux::Pane, procs: &Procs) -> bool {
    match pane.command.as_str() {
        "sh" => pane.exited,
        cmd if is_agent_command(cmd) => false,
        // Otro programa en primer plano puede estar envolviendo al agente
        // (headroom, por ejemplo, aparece como `python`).
        _ => !pane.pid.is_some_and(|pid| procs.runs_agent(pid)),
    }
}

/// Tabla de procesos del sistema. Se pide a `ps` una sola vez, y solo si algún
/// panel la necesita, para no lanzarlo por cada agente en cada refresco.
#[derive(Default)]
/// `None` si `ps` falló. La línea de órdenes de cada proceso, aparte y solo si
/// hace falta (hay un `node` o un `bun` en primer plano).
struct Procs(
    OnceCell<Option<Vec<Proc>>>,
    OnceCell<Option<HashMap<u32, String>>>,
);

struct Proc {
    pid: u32,
    pgid: i64,
    tpgid: i64,
    name: String,
}

impl Procs {
    fn list(&self) -> Option<&[Proc]> {
        self.0
            .get_or_init(|| {
                std::process::Command::new("ps")
                    .args(["-A", "-o", "pid=,pgid=,tpgid=,comm="])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
            })
            .as_deref()
    }

    /// Si el agente corre en primer plano en el panel que arrancó con `pid`:
    /// algún proceso del grupo que tiene la terminal es un agente. Así se le
    /// encuentra detrás de un envoltorio como headroom, y no cuenta uno que
    /// quedó en segundo plano. Si `ps` falló no se sabe, y se da por bueno:
    /// darlo por salido borraría su estado de hook.
    fn runs_agent(&self, pid: u32) -> bool {
        let Some(procs) = self.list() else {
            return true;
        };
        let Some(fg) = procs.iter().find(|p| p.pid == pid).map(|p| p.tpgid) else {
            return false;
        };
        fg > 0 && procs.iter().any(|p| p.pgid == fg && self.is_agent(p))
    }

    fn args(&self) -> Option<&HashMap<u32, String>> {
        self.1
            .get_or_init(|| {
                std::process::Command::new("ps")
                    .args(["-A", "-o", "pid=,args="])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| parse_ps_args(&String::from_utf8_lossy(&o.stdout)))
            })
            .as_ref()
    }

    /// Instalados con npm o bun, los agentes aparecen como su intérprete: hay
    /// que mirar qué script corre. Un `node` cualquiera (un REPL) no cuenta,
    /// o lo que se le mande al agente se ejecutaría ahí. Si `ps` falló, como
    /// en `runs_agent`, se da por bueno.
    fn is_agent(&self, p: &Proc) -> bool {
        if is_agent_command(&p.name) {
            return true;
        }
        if p.name != "node" && p.name != "bun" {
            return false;
        }
        self.args()
            .is_none_or(|args| args.get(&p.pid).is_some_and(|a| runs_agent_script(a)))
    }
}

/// Lee la salida de `ps -o pid=,args=`.
fn parse_ps_args(out: &str) -> HashMap<u32, String> {
    out.lines()
        .filter_map(|l| {
            let (pid, args) = l.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, args.trim().to_string()))
        })
        .collect()
}

/// Lee la salida de `ps -o pid=,pgid=,tpgid=,comm=`.
fn parse_ps(out: &str) -> Vec<Proc> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let pgid = f.next()?.parse().ok()?;
            let tpgid = f.next()?.parse().ok()?;
            // En macOS `comm` es la ruta completa.
            let comm = f.collect::<Vec<_>>().join(" ");
            let name = Path::new(&comm).file_name()?.to_string_lossy().into_owned();
            Some(Proc {
                pid,
                pgid,
                tpgid,
                name,
            })
        })
        .collect()
}

fn is_agent_command(cmd: &str) -> bool {
    Kind::ALL.iter().any(|k| k.bin() == cmd)
}

/// Si la línea de órdenes de un intérprete corre un agente: el script que
/// ejecuta (el primer argumento que no es una opción) es el suyo
/// (`.../bin/claude`, `.../codex.js`) o está en su paquete
/// (`node_modules/@anthropic-ai/claude-code/cli.js`). Lo que va después son
/// argumentos del script: `node app.js codex.json` no es un agente.
fn runs_agent_script(args: &str) -> bool {
    let agent = |name: &str| Kind::ALL.iter().any(|k| name.starts_with(k.bin()));
    let Some(script) = args
        .split_whitespace()
        .skip(1)
        .find(|a| !a.starts_with('-'))
    else {
        return false;
    };
    let parts: Vec<_> = Path::new(script)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    let package = parts.iter().position(|c| c == "node_modules");
    parts.last().is_some_and(|f| agent(f))
        || package.is_some_and(|i| parts[i + 1..].iter().any(|c| agent(c)))
}

/// Panel al que hay que hablarle para llegar al agente. Falla si el agente
/// no está corriendo: el texto llegaría al shell y se ejecutaría como comando.
fn agent_target(agent: &Agent) -> Result<String> {
    match agent_panes(&agent.id).agent {
        Some(p) if !p.dead && !agent_gone(&p, &Procs::default()) => Ok(p.id),
        _ => bail!(
            "el agente {} no está corriendo; entra a su terminal y vuelve a abrirlo",
            agent.name
        ),
    }
}

/// La sesión de tmux del agente, para entrar en ella desde otro lado. A
/// diferencia de `agent_target`, vale aunque el agente haya salido: entrar
/// es justo cómo se vuelve a abrir.
#[cfg(feature = "serve")]
pub fn attach_session(agent_key: &str) -> Result<String> {
    let state = State::load()?;
    let agent = state.find_agent(agent_key)?;
    if !tmux::panes()?.contains_key(&agent.id) {
        bail!("{} no tiene sesión de tmux; restáuralo primero", agent.name);
    }
    Ok(agent.id.clone())
}

pub fn send(agent_key: &str, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("el mensaje está vacío");
    }
    if let Some((host, id)) = remote::split(agent_key) {
        // Con `--`: un mensaje como `--help` es texto, no una opción de allá.
        return remote::run(&host, &["send", id, "--", text]).map(drop);
    }
    let state = State::load()?;
    let agent = state.find_agent(agent_key)?;
    tmux::send_text(&agent_target(agent)?, text)?;
    history::record(Event::MessageSent {
        agent: agent.id.clone(),
    });
    Ok(())
}

/// Pulsa teclas sueltas en el agente (Enter, Escape, flechas…), para los
/// menús que no se contestan con texto.
pub fn send_keys(agent_key: &str, keys: &[String]) -> Result<()> {
    tmux::check_keys(keys)?;
    if let Some((host, id)) = remote::split(agent_key) {
        let mut args = vec!["keys", id];
        args.extend(keys.iter().map(String::as_str));
        return remote::run(&host, &args).map(drop);
    }
    let state = State::load()?;
    tmux::send_keys(&agent_target(state.find_agent(agent_key)?)?, keys)
}

/// Lo que se ve ahora en el panel del agente. Si el agente ya salió, lo que
/// haya quedado en su lugar (el shell, por ejemplo). Sin los espacios con los
/// que tmux rellena cada línea hasta el ancho del panel.
pub fn screen(agent_key: &str) -> Result<String> {
    if let Some((host, id)) = remote::split(agent_key) {
        let out = remote::run(&host, &["screen", id])?;
        return Ok(out.strip_suffix('\n').unwrap_or(&out).to_string());
    }
    let state = State::load()?;
    let agent = state.find_agent(agent_key)?;
    // Con `?` y no `agent_panes`: si tmux falla, que se vea por qué.
    let panes = split_panes(tmux::panes()?.remove(&agent.id).unwrap_or_default());
    match panes.agent {
        Some(p) => Ok(tmux::capture(&p.id)?
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()),
        None => bail!("el agente {} no está corriendo", agent.name),
    }
}

/// Ajusta el panel del agente al tamaño de quien lo mira (la TUI o el
/// celular): el agente redibuja a ese tamaño y sus líneas no se salen de la
/// pantalla. Devuelve si cambió algo.
pub fn fit(agent_key: &str, cols: u16, rows: u16) -> Result<bool> {
    if let Some((host, id)) = remote::split(agent_key) {
        let (cols, rows) = (cols.to_string(), rows.to_string());
        remote::run(&host, &["screen", id, "--cols", &cols, "--rows", &rows])?;
        // Allá ya esperó a que se redibujara: aquí no hace falta otra pausa.
        return Ok(false);
    }
    let state = State::load()?;
    let pane = agent_target(state.find_agent(agent_key)?)?;
    tmux::fit(&pane, cols.clamp(20, 500), rows.clamp(5, 200))
}

/// Entra a la sesión del agente con el foco en el agente.
pub fn attach(agent_key: &str) -> Result<()> {
    if let Some((host, id)) = remote::split(agent_key) {
        return remote::interactive(&host, &["attach", id]);
    }
    let id = State::load()?.find_agent(agent_key)?.id.clone();
    let focus = agent_panes(&id).agent.map(|p| p.id);
    tmux::attach(&id, focus.as_deref())
}

/// Abre una terminal a la derecha del agente, en su mismo directorio, y
/// opcionalmente ejecuta `cmd` en ella (p. ej. "npm run dev").
pub fn open_term(agent_key: &str, cmd: Option<&str>) -> Result<()> {
    if let Some((host, id)) = remote::split(agent_key) {
        let mut args = vec!["term", id];
        // Uno solo: allá se pasa tal cual al shell, como aquí. Tras `--`, para
        // que uno como `--close` no se tome como opción.
        if let Some(c) = cmd.map(str::trim).filter(|c| !c.is_empty()) {
            args.extend(["--", c]);
        }
        return remote::run(&host, &args).map(drop);
    }
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
        // Solo para el título: no vale la pena fallar por ello.
        let _ = tmux::set_cmd(&term, c);
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
    if let Some((host, id)) = remote::split(agent_key) {
        return remote::run(&host, &["term", id, "--close"]).map(drop);
    }
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
    // Su directorio está en otra máquina: aquí no se puede mirar.
    if agent.host.is_some() {
        return String::new();
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
    if let Some((host, id)) = remote::split(agent_key) {
        return remote::run(&host, &["kill", id]).map(drop);
    }
    // Lo que gastó desde su último turno, todavía con su tarea.
    if let Ok(a) = State::load().and_then(|s| s.find_agent(agent_key).cloned()) {
        usage::flush_quietly(&a, None);
    }
    // Primero fuera del estado y después la sesión: si la TUI lo mirara entre
    // medias, lo vería muerto y tomaría su tarea por terminada. Se busca dentro
    // del candado: si dos lo matan a la vez, solo uno lo encuentra, y el fin
    // de la sesión no queda dos veces en el historial.
    let (agent, orphan) = update_recording(|s, mut record| {
        let agent = remove_agent(s, agent_key, &mut record)?;
        let orphan = agent.worktree.clone().filter(|wt| !in_use(s, wt));
        Ok((agent, orphan))
    })?;
    let killed = tmux::kill_session(&agent.id);
    hooks::clear_status(&agent.id);
    usage::forget(&agent.id);
    // Un worktree sin cambios y sin tarea que revisar no sirve para nada. Con
    // cambios se queda: que no se pierda trabajo.
    if let Some(wt) = &orphan {
        worktree::remove_if_unchanged(wt);
    }
    history::record(Event::AgentEnded {
        agent: agent.id.clone(),
        kind: agent.kind,
        secs: (Utc::now() - agent.created_at).num_seconds(),
    });
    // Si ya no estaba, bien; si sigue viva, que se sepa: fuera del estado ya
    // no se ve, y seguiría gastando.
    match killed {
        Err(e) if !tmux::session_gone(&e) => Err(e.context(format!(
            "lo quité de pastor, pero su sesión sigue viva (tmux -L pastor kill-session -t ={})",
            agent.id
        ))),
        _ => Ok(()),
    }
}

/// Vuelve a abrir un agente cuya sesión de tmux se perdió (por ejemplo, al
/// reiniciar la máquina): con el mismo id, en su directorio y retomando su
/// conversación. Su tarea sigue con él.
pub fn restore_agent(agent_key: &str) -> Result<Agent> {
    if let Some((host, id)) = remote::split(agent_key) {
        remote::run(&host, &["restore", id])?;
        // Como se ve desde aquí: con la máquina en el id.
        return remote::agents(&host)?
            .into_iter()
            .map(|(a, _)| a)
            .find(|a| a.id == format!("{host}:{id}") || a.name == id)
            .ok_or_else(|| anyhow::anyhow!("{host} restauró {id}, pero ya no lo encuentro"));
    }
    tmux::ensure_available()?;
    // Con el candado: que nadie lo quite ni lo restaure a la vez. Otro
    // `new-session` con el mismo nombre fallaría, y limpiar ese fallo mataría
    // la sesión que sí se abrió.
    State::with_lock(|s| {
        let agent = s.find_agent(agent_key)?.clone();
        let panes = split_panes(tmux::panes()?.remove(&agent.id).unwrap_or_default());
        if let Some(p) = panes.agent.as_ref().filter(|p| !p.dead) {
            if agent_gone(p, &Procs::default()) {
                bail!(
                    "{} salió, pero su terminal sigue abierta: entra y vuelve a abrir {}",
                    agent.name,
                    agent.kind
                );
            }
            bail!("{} ya está corriendo", agent.name);
        }
        relaunch(&agent)?;
        Ok(agent)
    })?
}

/// Restaura todos los agentes que perdieron su sesión de tmux. Devuelve cada
/// uno con lo que pasó al intentarlo.
pub fn restore_lost() -> Result<Vec<(Agent, Result<()>)>> {
    tmux::ensure_available()?;
    let mut panes = tmux::panes()?;
    let lost: Vec<Agent> = State::load()?
        .agents
        .into_iter()
        .filter(|a| {
            split_panes(panes.remove(&a.id).unwrap_or_default())
                .agent
                .is_none_or(|p| p.dead)
        })
        .collect();
    Ok(lost
        .into_iter()
        .map(|a| {
            let restored = restore_agent(&a.id).map(|_| ());
            (a, restored)
        })
        .collect())
}

fn relaunch(agent: &Agent) -> Result<()> {
    if !agent.cwd.is_dir() {
        bail!("su directorio {} ya no existe", agent.cwd.display());
    }
    // Las versiones que no guardaban la sesión la tienen en la transcripción
    // del último hook: `<sesión>.jsonl`.
    let session = agent.session.clone().or_else(|| {
        let path = PathBuf::from(hooks::read_status(&agent.id)?.transcript?);
        Some(path.file_stem()?.to_string_lossy().into_owned())
    });
    let cmd = agent.kind.resume_command(session.as_deref())?;
    // Un panel muerto o una terminal suelta: lo que quede de la sesión vieja.
    match tmux::kill_session(&agent.id) {
        Err(e) if !tmux::session_gone(&e) => return Err(e),
        _ => {}
    }
    start_session(agent, &cmd)
}

/// Abre la sesión de tmux del agente con `cmd`. Si falla, la sesión pudo
/// crearse antes: se cierra.
fn start_session(agent: &Agent, cmd: &[String]) -> Result<()> {
    hooks::clear_status(&agent.id);
    let started = hooks::relaunch_shell().and_then(|then| {
        tmux::new_session(
            &agent.id,
            &agent.cwd,
            &[("PASTOR_AGENT", &agent.id)],
            cmd,
            &then,
        )
    });
    if started.is_err() {
        let _ = tmux::kill_session(&agent.id);
    }
    started
}

/// Si algún agente o tarea sigue usando el worktree.
fn in_use(s: &State, wt: &Worktree) -> bool {
    s.agents.iter().any(|a| a.worktree.as_ref() == Some(wt))
        || s.tasks.iter().any(|t| t.worktree.as_ref() == Some(wt))
}

/// Saca al agente del estado y libera su tarea.
fn remove_agent(
    s: &mut State,
    agent_key: &str,
    record: &mut impl FnMut(&Task, TaskStatus),
) -> Result<Agent> {
    let agent = s.find_agent(agent_key)?.clone();
    s.agents.retain(|a| a.id != agent.id);
    for t in s
        .tasks
        .iter_mut()
        .filter(|t| t.agent.as_deref() == Some(&agent.id))
    {
        release(t, record);
    }
    Ok(agent)
}

pub enum AssignTarget {
    Existing(String),
    /// Sin `cwd`, en el directorio actual. Con `worktree`, en uno nuevo; si
    /// la tarea ya tiene el suyo (se asignó antes), en ese.
    New {
        kind: Kind,
        cwd: Option<PathBuf>,
        worktree: bool,
    },
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
    let (agent, created) = match target {
        AssignTarget::Existing(key) => {
            let a = state.find_agent(&key)?.clone();
            // Lo que gastó hasta ahora es de la tarea que tenía.
            usage::flush_quietly(&a, None);
            tmux::send_text(&agent_target(&a)?, &prompt)?;
            (a, false)
        }
        AssignTarget::New {
            kind,
            cwd,
            worktree,
        } => {
            let place = match task.worktree.clone() {
                _ if !worktree => Place::Dir,
                Some(wt) if wt.path.is_dir() => {
                    // Dos agentes en la misma copia se pisarían.
                    if let Some(a) = state
                        .agents
                        .iter()
                        .find(|a| a.worktree.as_ref() == Some(&wt))
                    {
                        bail!(
                            "{} sigue trabajando en el worktree de {}; termínalo o asígnasela a él",
                            a.name,
                            task.id
                        );
                    }
                    Place::Worktree(wt)
                }
                _ => Place::NewWorktree(Some(format!("{}-{}", task.id, task.title))),
            };
            (new_agent(kind, cwd, None, Some(&prompt), place)?, true)
        }
    };
    // Lo que pasó entre leer la tarea y ahora (otra asignación, la tarea
    // borrada o cerrada) lo resuelve `link_task` dentro del candado.
    let linked = update_recording(|s, mut record| {
        link_task(s, &task.id, &agent.id, task.status, &mut record)?;
        // La tarea recuerda el worktree del agente para revisarla aunque el
        // agente ya no esté. Sin worktree se revisa a mano, en su directorio.
        let t = s.find_task_mut(&task.id)?;
        let old = std::mem::replace(&mut t.worktree, agent.worktree.clone());
        Ok(old.filter(|wt| !in_use(s, wt)))
    });
    // El worktree que la tarea deja, si nadie más lo usa: sin cambios se
    // borra; con cambios se queda en disco, como al terminar un agente.
    if let Ok(Some(old)) = &linked {
        worktree::remove_if_unchanged(old);
    }
    if let Err(e) = linked {
        // Un agente lanzado solo para esta tarea no tiene nada que hacer.
        if created {
            let _ = kill_agent(&agent.id);
            bail!("{e:#}; se terminó {}", agent.name);
        }
        bail!("{e:#}; {} ya recibió el texto", agent.name);
    }
    Ok(agent)
}

/// Deja `task_id` en curso con `agent_id`, y solo con él. `seen` es el estado
/// de la tarea cuando se pidió asignarla. `record` anota cada cambio de estado
/// antes de hacerlo.
fn link_task(
    s: &mut State,
    task_id: &str,
    agent_id: &str,
    seen: TaskStatus,
    record: &mut impl FnMut(&Task, TaskStatus),
) -> Result<()> {
    // Lo pudieron matar desde otro proceso mientras se le mandaba el prompt.
    if !s.agents.iter().any(|a| a.id == agent_id) {
        bail!("el agente {agent_id} ya no existe");
    }
    match s.find_task(task_id) {
        Err(_) => bail!("la tarea {task_id} se borró mientras se asignaba"),
        // Alguien la cerró entre medias: eso vale más que reabrirla. Asignar
        // una tarea que ya estaba cerrada sí la reabre.
        Ok(t) if t.status != seen && matches!(t.status, TaskStatus::Review | TaskStatus::Done) => {
            bail!(
                "la tarea {task_id} pasó a «{}» mientras se asignaba",
                t.status.label()
            )
        }
        Ok(_) => {}
    }
    let mut previous = None;
    for a in &mut s.agents {
        if a.id == agent_id {
            previous = a.task.replace(task_id.to_string());
        } else if a.task.as_deref() == Some(task_id) {
            // El agente anterior ya no la lleva: que su próximo turno no la toque.
            a.task = None;
        }
    }
    // La tarea que el agente dejó a medias vuelve a quedar libre.
    if let Some(p) = previous.filter(|p| p != task_id) {
        if let Ok(t) = s.find_task_mut(&p) {
            if t.status == TaskStatus::InProgress && t.agent.as_deref() == Some(agent_id) {
                release(t, record);
            }
        }
    }
    let t = s.find_task_mut(task_id)?;
    t.agent = Some(agent_id.to_string());
    record(t, TaskStatus::InProgress);
    t.status = TaskStatus::InProgress;
    Ok(())
}

/// El agente terminó su turno: si su tarea sigue en curso, pasa a revisión.
/// Todo se mira dentro del candado, para no pisar un cambio recién hecho por
/// otro proceso (la tarea ya hecha, o reasignada a otro agente).
pub fn finish_turn(agent_id: &str) -> Result<()> {
    update_recording(|s, mut record| {
        finish_turn_in(s, agent_id, &mut record);
        Ok(())
    })
}

/// La tarea se queda sin agente; si estaba en curso, vuelve a pendiente.
fn release(t: &mut Task, record: &mut impl FnMut(&Task, TaskStatus)) {
    t.agent = None;
    if t.status == TaskStatus::InProgress {
        record(t, TaskStatus::Todo);
        t.status = TaskStatus::Todo;
    }
}

fn finish_turn_in(
    s: &mut State,
    agent_id: &str,
    record: &mut impl FnMut(&Task, TaskStatus),
) -> Option<String> {
    let task = s.agents.iter().find(|a| a.id == agent_id)?.task.clone()?;
    let t = s.tasks.iter_mut().find(|t| t.id == task)?;
    // Sin `agent` (datos de antes de que se guardara) basta con `agent.task`.
    let ours = t.agent.as_deref().is_none_or(|a| a == agent_id);
    if t.status != TaskStatus::InProgress || !ours {
        return None;
    }
    record(t, TaskStatus::Review);
    t.status = TaskStatus::Review;
    Some(task)
}

pub fn set_task_status(task_id: &str, status: TaskStatus) -> Result<()> {
    update_recording(|s, record| {
        let t = s.find_task_mut(task_id)?;
        record(t, status);
        t.status = status;
        Ok(())
    })
}

/// Como `State::update`, pero `f` recibe con qué anotar los cambios de
/// estado de las tareas, y solo pasan al historial si el estado se guardó
/// (con el candado aún tomado, para que queden en el mismo orden).
fn update_recording<T>(
    f: impl FnOnce(&mut State, &mut dyn FnMut(&Task, TaskStatus)) -> Result<T>,
) -> Result<T> {
    let (out, _) = State::update_then(
        |s| {
            let mut events = Vec::new();
            let out = f(s, &mut |task: &Task, to| {
                // Solo si de verdad cambia.
                if task.status != to {
                    events.push(Event::TaskStatus {
                        task: task.id.clone(),
                        to,
                        secs: (Utc::now() - task.created_at).num_seconds(),
                    });
                }
            })?;
            Ok((out, events))
        },
        |(_, events)| events.drain(..).for_each(history::record),
    )?;
    Ok(out)
}

pub fn add_task(title: String, description: String) -> Result<Task> {
    let task = State::update(|s| Ok(s.add_task(title, description)))?;
    history::record(Event::TaskCreated {
        task: task.id.clone(),
    });
    Ok(task)
}

pub fn delete_task(task_id: &str) -> Result<()> {
    let orphan = State::update(|s| {
        let wt = s.find_task(task_id)?.worktree.clone();
        s.tasks.retain(|t| t.id != task_id);
        for a in s
            .agents
            .iter_mut()
            .filter(|a| a.task.as_deref() == Some(task_id))
        {
            a.task = None;
        }
        Ok(wt.filter(|wt| !in_use(s, wt)))
    })?;
    if let Some(wt) = &orphan {
        worktree::remove_if_unchanged(wt);
    }
    history::record(Event::TaskDeleted {
        task: task_id.to_string(),
    });
    Ok(())
}

/// Con quién comparte archivos cambiados el trabajo de una tarea o agente.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum With {
    Task(String),
    /// Un agente con worktree que no tiene tarea.
    Agent(String),
    /// La rama actual del repositorio y lo que tienes sin guardar en él.
    Repo,
}

impl With {
    /// `t4 (web)`, `el agente web` o `tu rama`.
    pub fn label(&self, state: &State) -> String {
        let name = |id: &str| {
            state
                .agents
                .iter()
                .find(|a| a.id == id)
                .map_or_else(|| id.to_string(), |a| a.name.clone())
        };
        match self {
            With::Task(t) => match state.find_task(t).ok().and_then(|t| t.agent.as_deref()) {
                Some(a) => format!("{t} ({})", name(a)),
                None => t.clone(),
            },
            With::Agent(a) => format!("el agente {}", name(a)),
            With::Repo => "tu rama".to_string(),
        }
    }
}

/// Archivos que cambió también otro.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub with: With,
    pub files: Vec<String>,
}

/// Los archivos que cada tarea en curso (por su id) o agente con worktree y
/// sin tarea (por el suyo) cambió y que también cambió otra tarea, otro
/// agente o tu rama del mismo repositorio. Al integrar, ahí puede haber
/// conflictos. Solo los que tienen alguno.
pub fn overlaps(state: &State) -> HashMap<String, Vec<Overlap>> {
    // Cada worktree una vez, con quién lo usa.
    let tasks = state
        .tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Done)
        .filter_map(|t| Some((With::Task(t.id.clone()), t.worktree.as_ref()?)));
    let agents = state
        .agents
        .iter()
        .filter(|a| a.host.is_none())
        .filter_map(|a| Some((With::Agent(a.id.clone()), a.worktree.as_ref()?)));
    let mut owners: Vec<(With, Worktree)> = Vec::new();
    for (w, wt) in tasks.chain(agents) {
        if !owners.iter().any(|(_, seen)| seen == wt) {
            owners.push((w, wt.clone()));
        }
    }
    // La base se calcula una vez por worktree: la usan los dos lados.
    let touched: Vec<(With, Worktree, String, Vec<String>)> = owners
        .into_iter()
        .filter(|(_, wt)| wt.path.is_dir())
        .filter_map(|(w, wt)| {
            let base = worktree::diff_base(&wt);
            let files = worktree::touched(&wt, &base).ok()?;
            (!files.is_empty()).then_some((w, wt, base, files))
        })
        .collect();
    let mut out: HashMap<String, Vec<Overlap>> = HashMap::new();
    let key = |w: &With| match w {
        With::Task(id) | With::Agent(id) => id.clone(),
        With::Repo => String::new(),
    };
    let common = |a: &[String], b: &[String]| -> Vec<String> {
        a.iter()
            .filter(|f| b.binary_search(f).is_ok())
            .cloned()
            .collect()
    };
    for (i, (w, wt, base, files)) in touched.iter().enumerate() {
        for (w2, wt2, _, files2) in touched.iter().skip(i + 1) {
            if wt.repo != wt2.repo {
                continue;
            }
            let both = common(files, files2);
            if both.is_empty() {
                continue;
            }
            out.entry(key(w)).or_default().push(Overlap {
                with: w2.clone(),
                files: both.clone(),
            });
            out.entry(key(w2)).or_default().push(Overlap {
                with: w.clone(),
                files: both,
            });
        }
        if let Ok(repo) = worktree::repo_touched(wt, base) {
            let both = common(files, &repo);
            if !both.is_empty() {
                out.entry(key(w)).or_default().push(Overlap {
                    with: With::Repo,
                    files: both,
                });
            }
        }
    }
    out
}

/// `src/a.rs y src/b.rs con t4 (web); src/c.rs con tu rama`, para avisar.
pub fn describe_overlaps(state: &State, list: &[Overlap]) -> String {
    list.iter()
        .map(|o| {
            let files = match o.files.as_slice() {
                [one] => one.clone(),
                [a, b] => format!("{a} y {b}"),
                [a, rest @ ..] => format!("{a} y {} más", rest.len()),
                [] => String::new(),
            };
            format!("{files} con {}", o.with.label(state))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// El worktree de una tarea, para ver lo que cambió.
pub fn task_worktree(task_id: &str) -> Result<Worktree> {
    let state = State::load()?;
    let task = state.find_task(task_id)?;
    task.worktree
        .clone()
        .ok_or_else(|| anyhow::anyhow!("la tarea {task_id} no tiene worktree: se asignó sin uno"))
}

/// Da la tarea por buena: guarda en un commit lo que quedó sin guardar,
/// integra su rama en la rama actual del repositorio, termina al agente que
/// trabajaba en el worktree y lo borra. Sin worktree solo la marca hecha.
pub fn approve_task(task_id: &str) -> Result<String> {
    let task = State::load()?.find_task(task_id)?.clone();
    let Some(wt) = task.worktree.clone() else {
        set_task_status(task_id, TaskStatus::Done)?;
        return Ok(format!("Tarea {task_id} hecha"));
    };
    // Integrar el worktree también integraría el trabajo de la otra tarea.
    if let Some(other) = State::load()?.tasks.iter().find(|t| {
        t.id != task.id && t.status != TaskStatus::Done && t.worktree.as_ref() == Some(&wt)
    }) {
        bail!(
            "el worktree de {task_id} también lo usa la tarea {}; termina esa primero o bórrala",
            other.id
        );
    }
    let msg = format!("[{}] {}", task.id, task.title);
    // Primero con el agente vivo: si hay conflictos, todavía puede resolverlos.
    let into = worktree::merge(&wt, &msg)?;
    // Su carpeta está por desaparecer.
    for a in State::load()?
        .agents
        .iter()
        .filter(|a| a.worktree.as_ref() == Some(&wt))
    {
        kill_agent(&a.id)?;
    }
    // Lo que el agente alcanzó a escribir antes de terminarlo.
    let into = worktree::merge(&wt, &msg)?.or(into);
    worktree::remove(&wt)?;
    update_recording(|s, record| {
        let t = s.find_task_mut(task_id)?;
        t.worktree = None;
        record(t, TaskStatus::Done);
        t.status = TaskStatus::Done;
        Ok(())
    })?;
    Ok(match into {
        Some(into) => format!("Tarea {task_id} integrada en {into}"),
        None => format!("Tarea {task_id} hecha; no tenía cambios"),
    })
}

/// Devuelve la tarea a su agente con lo que hay que cambiar, y vuelve a
/// quedar en curso.
pub fn reject_task(task_id: &str, comment: &str) -> Result<Agent> {
    if comment.trim().is_empty() {
        bail!("escribe qué hay que cambiar");
    }
    let state = State::load()?;
    let task = state.find_task(task_id)?;
    let agent = task
        .agent
        .as_deref()
        .and_then(|id| state.agents.iter().find(|a| a.id == id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "la tarea {task_id} ya no tiene agente; asígnala de nuevo (se usa su mismo worktree)"
            )
        })?;
    let text = format!(
        "[{}] Cambios pedidos en la revisión:\n\n{}",
        task.id,
        comment.trim()
    );
    tmux::send_text(&agent_target(agent)?, &text)?;
    history::record(Event::MessageSent {
        agent: agent.id.clone(),
    });
    update_recording(|s, record| {
        let t = s.find_task_mut(task_id)?;
        // Solo si sigue siendo suya: si no, el mensaje llegó pero la tarea
        // ya es de otro.
        if t.agent.as_deref() == Some(&agent.id) {
            record(t, TaskStatus::InProgress);
            t.status = TaskStatus::InProgress;
        }
        Ok(())
    })?;
    Ok(agent.clone())
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
    fn observe(&mut self, id: &str, hash: u64, size: &str) -> Option<DateTime<Utc>> {
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
    /// Comando con el que se abrió esta vez, si se abrió con uno.
    pub cmd: Option<String>,
}

/// Falla si no se pudo preguntar a tmux por los paneles.
pub fn observe(state: &State, tracker: &mut Tracker) -> Result<HashMap<String, Snapshot>> {
    let mut all = tmux::panes()?;
    // Que no recuerde para siempre a los agentes que ya no están (`pastor
    // serve` puede correr semanas).
    tracker
        .seen
        .retain(|id, _| state.agents.iter().any(|a| &a.id == id));
    let procs = Procs::default();
    // Primero si cada agente sigue en su panel, antes de las capturas, para que
    // `ps` vea lo mismo que acaba de reportar tmux.
    let panes: Vec<_> = state
        .agents
        .iter()
        .map(|a| {
            let panes = split_panes(all.remove(&a.id).unwrap_or_default());
            let gone = panes
                .agent
                .as_ref()
                .is_some_and(|p| !p.dead && agent_gone(p, &procs));
            (a, panes, gone)
        })
        .collect();
    Ok(panes
        .into_iter()
        .map(|(a, panes, gone)| {
            let pane = panes.agent.as_ref();
            let screen = match pane {
                Some(p) => tmux::capture(&p.id).unwrap_or_default(),
                None => String::new(),
            };
            let term = panes.term.map(|t| TermSnapshot {
                screen: tmux::capture(&t.id).unwrap_or_default(),
                dead: t.dead,
                cmd: Some(t.cmd).filter(|c| !c.is_empty()),
            });
            let size = pane.map_or("", |p| p.size.as_str());
            // En Claude Code lo que pasa en el pie (escribir en el prompt, las
            // pistas que pone y quita) no es actividad del agente.
            let fingerprint = hooks::fingerprint(&screen);
            let hash = match a.kind {
                Kind::Claude => fingerprint,
                _ => hooks::activity_hash(&screen),
            };
            let obs = Observation {
                dead: pane.is_none_or(|p| p.dead),
                gone,
                screen: &screen,
                fingerprint,
                size,
                last_change: tracker.observe(&a.id, hash, size),
                age_secs: (Utc::now() - a.created_at).num_seconds(),
            };
            // Si lo vuelven a abrir a mano, que no herede el estado de la vez anterior.
            if obs.gone {
                hooks::clear_status(&a.id);
            }
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
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_parpadeo_del_punto_no_es_actividad() {
        let mut t = Tracker::default();
        let obs =
            |t: &mut Tracker, screen: &str| t.observe("a1", hooks::fingerprint(screen), "80x24");
        let pie = "\n\n───\n❯ \n───\n  ? for shortcuts";
        assert_eq!(
            obs(
                &mut t,
                &format!("● Bash(curl)\nDo you want to proceed?{pie}")
            ),
            None
        );
        assert_eq!(
            obs(
                &mut t,
                &format!("  Bash(curl)\nDo you want to proceed?{pie}")
            ),
            None
        );
        assert_eq!(
            obs(
                &mut t,
                &format!("⏺ Bash(curl)\nDo you want to proceed?{pie}")
            ),
            None
        );
        assert!(obs(&mut t, &format!("  Bash(curl)\n⎿ Interrupted{pie}")).is_some());
    }

    #[test]
    fn escribir_en_el_prompt_no_es_actividad() {
        let mut t = Tracker::default();
        let obs =
            |t: &mut Tracker, screen: &str| t.observe("a1", hooks::fingerprint(screen), "80x24");
        let pantalla =
            |prompt: &str| format!("● Listo.\n\n───\n❯ {prompt}\n───\n  ? for shortcuts");
        assert_eq!(obs(&mut t, &pantalla("")), None);
        assert_eq!(obs(&mut t, &pantalla("arregla el test")), None);
    }

    /// Dos agentes (a1, a2) y dos tareas (t1, t2), sin tocar el historial.
    fn two_and_two() -> State {
        let mut s = State::default();
        for _ in 0..2 {
            s.add_agent(Kind::Claude, PathBuf::from("/tmp"), None);
            s.add_task("tarea".into(), String::new());
        }
        s
    }

    fn link(s: &mut State, task: &str, agent: &str) {
        let seen = status(s, task);
        link_task(s, task, agent, seen, &mut |_, _| {}).unwrap();
    }

    fn turn(s: &mut State, agent: &str) -> Option<String> {
        finish_turn_in(s, agent, &mut |_, _| {})
    }

    fn status(s: &State, task: &str) -> TaskStatus {
        s.find_task(task).unwrap().status
    }

    #[test]
    fn reasignar_desliga_al_agente_anterior() {
        let mut s = two_and_two();
        link(&mut s, "t1", "a1");
        link(&mut s, "t1", "a2");
        assert_eq!(s.find_agent("a1").unwrap().task, None);
        // El turno de a1 ya no manda t1 a revisión mientras a2 trabaja en ella.
        assert_eq!(turn(&mut s, "a1"), None);
        assert_eq!(status(&s, "t1"), TaskStatus::InProgress);
        assert_eq!(turn(&mut s, "a2").as_deref(), Some("t1"));
        assert_eq!(status(&s, "t1"), TaskStatus::Review);
    }

    #[test]
    fn asignar_otra_tarea_libera_la_anterior() {
        let mut s = two_and_two();
        link(&mut s, "t1", "a1");
        link(&mut s, "t2", "a1");
        let t1 = s.find_task("t1").unwrap();
        assert_eq!((t1.status, t1.agent.clone()), (TaskStatus::Todo, None));
        assert_eq!(s.find_agent("a1").unwrap().task.as_deref(), Some("t2"));
    }

    #[test]
    fn no_asigna_a_un_agente_que_ya_no_existe() {
        let mut s = two_and_two();
        assert!(link_task(&mut s, "t1", "a9", TaskStatus::Todo, &mut |_, _| {}).is_err());
        assert_eq!(status(&s, "t1"), TaskStatus::Todo);
    }

    #[test]
    fn el_turno_vale_con_tareas_sin_agente_guardado() {
        let mut s = two_and_two();
        link(&mut s, "t1", "a1");
        s.find_task_mut("t1").unwrap().agent = None;
        assert_eq!(turn(&mut s, "a1").as_deref(), Some("t1"));
    }

    #[test]
    fn el_turno_no_toca_una_tarea_que_ya_no_esta_en_curso() {
        let mut s = two_and_two();
        link(&mut s, "t1", "a1");
        s.find_task_mut("t1").unwrap().status = TaskStatus::Done;
        assert_eq!(turn(&mut s, "a1"), None);
        assert_eq!(status(&s, "t1"), TaskStatus::Done);
    }

    #[test]
    fn dos_asignaciones_a_la_vez_dejan_la_tarea_con_la_ultima() {
        // Las dos leyeron el estado antes de que ninguna enlazara: cada
        // enlace se hace dentro del candado y el último manda.
        let mut s = two_and_two();
        link(&mut s, "t2", "a2");
        link(&mut s, "t1", "a1");
        link(&mut s, "t1", "a2");
        assert_eq!(s.find_agent("a1").unwrap().task, None);
        assert_eq!(s.find_agent("a2").unwrap().task.as_deref(), Some("t1"));
        assert_eq!(s.find_task("t1").unwrap().agent.as_deref(), Some("a2"));
        // La que a2 llevaba antes queda libre.
        let t2 = s.find_task("t2").unwrap();
        assert_eq!((t2.status, t2.agent.clone()), (TaskStatus::Todo, None));
    }

    #[test]
    fn asignar_una_tarea_borrada_entre_medias_no_toca_nada() {
        let mut s = two_and_two();
        link(&mut s, "t2", "a1");
        s.tasks.retain(|t| t.id != "t1");
        let err = link_task(&mut s, "t1", "a1", TaskStatus::Todo, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("se borró"), "{err}");
        assert_eq!(s.find_agent("a1").unwrap().task.as_deref(), Some("t2"));
        assert_eq!(status(&s, "t2"), TaskStatus::InProgress);
    }

    #[test]
    fn no_reabre_una_tarea_cerrada_mientras_se_asignaba() {
        let mut s = two_and_two();
        s.find_task_mut("t1").unwrap().status = TaskStatus::Done;
        let err = link_task(&mut s, "t1", "a1", TaskStatus::Todo, &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("hecha"), "{err}");
        assert_eq!(status(&s, "t1"), TaskStatus::Done);
        assert_eq!(s.find_agent("a1").unwrap().task, None);
        // Si ya estaba cerrada al pedirlo, asignarla la reabre.
        link(&mut s, "t1", "a1");
        assert_eq!(status(&s, "t1"), TaskStatus::InProgress);
    }

    #[test]
    fn matar_dos_veces_solo_cuenta_una() {
        let mut s = two_and_two();
        link(&mut s, "t1", "a1");
        let mut changes = Vec::new();
        let a = remove_agent(&mut s, "a1", &mut |t, to| changes.push((t.id.clone(), to))).unwrap();
        assert_eq!(a.id, "a1");
        assert_eq!(changes, [("t1".to_string(), TaskStatus::Todo)]);
        assert_eq!(s.find_task("t1").unwrap().agent, None);
        // El segundo ya no lo encuentra: ni sesión que matar ni otro fin anotado.
        assert!(remove_agent(&mut s, "a1", &mut |_, _| panic!("otro cambio")).is_err());
    }

    fn procs(list: &[(u32, i64, i64, &str)]) -> Procs {
        let list: Vec<_> = list
            .iter()
            .map(|&(pid, pgid, tpgid, name)| Proc {
                pid,
                pgid,
                tpgid,
                name: name.to_string(),
            })
            .collect();
        Procs(OnceCell::from(Some(list)), OnceCell::new())
    }

    #[test]
    fn agente_detras_de_envoltorio_en_primer_plano() {
        // zsh (10) cede la terminal al grupo 20: headroom y claude.
        let p = procs(&[
            (10, 10, 20, "zsh"),
            (20, 20, 20, "python"),
            (21, 20, 20, "claude"),
        ]);
        assert!(p.runs_agent(10));
    }

    #[test]
    fn agente_en_segundo_plano_no_cuenta() {
        // claude quedó en el grupo 20, pero la terminal la tiene vim (30).
        let p = procs(&[
            (10, 10, 30, "zsh"),
            (20, 20, 30, "claude"),
            (30, 30, 30, "vim"),
        ]);
        assert!(!p.runs_agent(10));
    }

    #[test]
    fn lee_ps_de_linux_y_macos() {
        let out = "    1     1    -1 systemd\n\
                   4242  4242  4250 zsh\n\
                   4250  4250  4250 /Users/yo/.local/bin/claude\n\
                   4251  4250  4250 Google Chrome Helper\n\
                   basura\n";
        let procs = parse_ps(out);
        let got: Vec<_> = procs
            .iter()
            .map(|p| (p.pid, p.pgid, p.tpgid, p.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (1, 1, -1, "systemd"),
                (4242, 4242, 4250, "zsh"),
                (4250, 4250, 4250, "claude"),
                (4251, 4250, 4250, "Google Chrome Helper"),
            ]
        );
        assert!(Procs(OnceCell::from(Some(procs)), OnceCell::new()).runs_agent(4242));
        // Si `ps` falló no se sabe: mejor no darlo por salido.
        assert!(Procs(OnceCell::from(None), OnceCell::new()).runs_agent(4242));
    }

    #[test]
    fn un_interprete_solo_cuenta_si_corre_un_agente() {
        assert!(runs_agent_script(
            "node /home/yo/.npm-global/bin/claude --settings x"
        ));
        assert!(runs_agent_script(
            "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        ));
        assert!(runs_agent_script("bun /home/yo/.bun/bin/opencode"));
        assert!(runs_agent_script(
            "node /usr/lib/node_modules/@openai/codex/bin/codex.js"
        ));
        assert!(!runs_agent_script("node"));
        assert!(!runs_agent_script("node servidor.js"));
        assert!(!runs_agent_script("node /home/yo/claude-notas/servidor.js"));
        assert!(!runs_agent_script("node servidor.js --config codex.json"));
        assert!(!runs_agent_script("node app.js claude-notas.md"));
        assert!(runs_agent_script(
            "node --no-warnings /usr/lib/node_modules/@openai/codex/bin/codex.js"
        ));
        let args = parse_ps_args("  42 node /x/bin/claude -c\n   7 /sbin/init\nbasura\n");
        assert_eq!(args[&42], "node /x/bin/claude -c");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn panel_desconocido() {
        assert!(!procs(&[]).runs_agent(10));
    }
}
