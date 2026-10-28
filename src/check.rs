//! Revisión automática de las tareas: al terminar el agente, pastor corre en
//! el worktree de la tarea el comando del proyecto (`git config pastor.check
//! "cargo test"`). Si falla, le devuelve la tarea al agente con la salida,
//! hasta `MAX_RETURNS` veces seguidas; después la deja por revisar para ti.
//!
//! Corre en un proceso aparte (`pastor task check <tarea> --return`), que
//! lanza quien pasa la tarea a revisión: el hook de Claude Code o el que
//! manda los avisos. Así no frena a nadie y sigue aunque se cierre la TUI.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::history::{self, Event};
use crate::store::{data_dir, Check, CheckState, State, TaskStatus};
use crate::{hooks, tmux, worktree};

/// Cuántas veces seguidas se le devuelve una tarea al agente.
pub const MAX_RETURNS: u32 = 3;
/// Lo que puede tardar el comando antes de darlo por fallido.
pub const TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Líneas de la salida que se guardan en la tarea y se le mandan al agente.
const TAIL_LINES: usize = 40;
/// Y como mucho tantos bytes: una línea puede ser enorme.
const TAIL_BYTES: usize = 4000;

/// El comando de revisión del repositorio de `dir`, si se configuró.
pub fn command_for(dir: &Path) -> Option<String> {
    worktree::config_get(dir, "pastor.check").filter(|c| !c.trim().is_empty())
}

/// Dónde se revisa la tarea: su worktree o, sin él, el directorio de su
/// agente. `None` si no hay dónde.
fn dir_for(state: &State, task_id: &str) -> Option<PathBuf> {
    let t = state.find_task(task_id).ok()?;
    let dir = match &t.worktree {
        Some(wt) => wt.path.clone(),
        None => {
            let agent = t.agent.as_deref()?;
            state.agents.iter().find(|a| a.id == agent)?.cwd.clone()
        }
    };
    dir.is_dir().then_some(dir)
}

/// Si la tarea tiene un comando de revisión, la revisa en otro proceso sin
/// esperarlo. Si falla, se la devuelve a su agente.
pub fn spawn_auto(task_id: &str) {
    let Ok(state) = State::load() else { return };
    if dir_for(&state, task_id)
        .and_then(|d| command_for(&d))
        .is_some()
    {
        hooks::spawn_pastor(&["task", "check", task_id, "--return"], &[]);
    }
}

/// Revisa la tarea en otro proceso, sin devolvérsela al agente si falla.
pub fn spawn_manual(task_id: &str) -> Result<String> {
    let state = State::load()?;
    let cmd = command(&state, task_id)?.1;
    hooks::spawn_pastor(&["task", "check", task_id], &[]);
    Ok(cmd)
}

/// Dónde y con qué se revisa la tarea, o por qué no se puede.
fn command(state: &State, task_id: &str) -> Result<(PathBuf, String)> {
    state.find_task(task_id)?;
    let Some(dir) = dir_for(state, task_id) else {
        bail!("la tarea {task_id} no tiene worktree ni agente donde revisarla");
    };
    let Some(cmd) = command_for(&dir) else {
        bail!(
            "su repositorio no tiene comando de revisión; configúralo con \
             `git -C {} config pastor.check \"cargo test\"`",
            dir.display()
        );
    };
    Ok((dir, cmd))
}

pub fn log_path(task_id: &str) -> PathBuf {
    data_dir().join("checks").join(format!("{task_id}.log"))
}

/// Cómo terminó una revisión.
pub struct Outcome {
    pub passed: bool,
    /// A quién se le devolvió la tarea, si se le devolvió.
    pub returned_to: Option<String>,
}

/// Revisa la tarea: corre su comando, guarda el resultado en ella y, con
/// `give_back`, si falla se la devuelve a su agente. `echo` recibe cada
/// línea de la salida mientras sale.
pub fn run(task_id: &str, give_back: bool, mut echo: impl FnMut(&str)) -> Result<Outcome> {
    let (dir, cmd) = command(&State::load()?, task_id)?;
    let started = Utc::now();
    State::update(|s| {
        let t = s.find_task_mut(task_id)?;
        // Dos a la vez correrían en la misma copia y se pisarían el log.
        if t.check.as_ref().is_some_and(Check::running) {
            bail!("la revisión de {task_id} ya está corriendo");
        }
        t.check = Some(Check {
            state: CheckState::Running,
            cmd: cmd.clone(),
            started,
            finished: None,
            output: String::new(),
            returned: false,
        });
        Ok(())
    })?;
    let log = log_path(task_id);
    fs::create_dir_all(log.parent().unwrap_or(Path::new(".")))?;
    let result = execute(&dir, &cmd, &log, TIMEOUT, &mut echo);
    // Si falló pastor (no pudo correr `sh`, ni escribir el log), el agente
    // no tiene nada que arreglar: no se le devuelve.
    let (passed, output, give_back) = match result {
        Ok((ok, tail)) => (ok, tail, give_back),
        Err(e) => (
            false,
            format!("pastor no pudo correr la revisión: {e:#}"),
            false,
        ),
    };
    // Solo si sigue siendo la misma revisión: si mientras se lanzó otra, o
    // se borró la tarea, este resultado ya no dice nada.
    let returned = crate::ops::update_recording(|s, record| {
        let Ok(t) = s.find_task_mut(task_id) else {
            return Ok(None);
        };
        if t.check.as_ref().map(|c| c.started) != Some(started) {
            return Ok(None);
        }
        t.check = Some(Check {
            state: if passed {
                CheckState::Passed
            } else {
                CheckState::Failed
            },
            cmd: cmd.clone(),
            started,
            finished: Some(Utc::now()),
            output: output.clone(),
            returned: false,
        });
        if passed {
            t.check_returns = 0;
            return Ok(None);
        }
        // Se le devuelve solo si sigue esperando revisión con su agente:
        // si ya la aprobaste o el agente siguió con ella, no.
        let agent = t.agent.clone();
        if !give_back || t.status != TaskStatus::Review || t.check_returns >= MAX_RETURNS {
            return Ok(None);
        }
        let Some(agent) = agent.and_then(|id| s.agents.iter().find(|a| a.id == id).cloned()) else {
            return Ok(None);
        };
        let t = s.find_task_mut(task_id)?;
        t.check_returns += 1;
        record(t, TaskStatus::InProgress);
        t.status = TaskStatus::InProgress;
        if let Some(c) = &mut t.check {
            c.returned = true;
        }
        Ok(Some((agent, t.check_returns)))
    })?;
    let Some((agent, n)) = returned else {
        return Ok(Outcome {
            passed,
            returned_to: None,
        });
    };
    let text = format!(
        "[{task_id}] La revisión automática (`{cmd}`) falló (intento {n} de {MAX_RETURNS}). \
         Arréglalo. Las últimas líneas de la salida (la completa está en {}):\n\n{output}",
        log.display()
    );
    let sent = crate::ops::agent_target(&agent).and_then(|pane| tmux::send_text(&pane, &text));
    if let Err(e) = sent {
        // No le llegó: que quede por revisar, para ti.
        let _ = crate::ops::update_recording(|s, record| {
            let t = s.find_task_mut(task_id)?;
            if t.status == TaskStatus::InProgress {
                record(t, TaskStatus::Review);
                t.status = TaskStatus::Review;
                t.check_returns = t.check_returns.saturating_sub(1);
            }
            if let Some(c) = t.check.as_mut().filter(|c| c.started == started) {
                c.returned = false;
            }
            Ok(())
        });
        return Err(e.context(format!("no pude devolverle la tarea a {}", agent.name)));
    }
    history::record(Event::MessageSent {
        agent: agent.id.clone(),
    });
    Ok(Outcome {
        passed,
        returned_to: Some(agent.name),
    })
}

/// Corre `cmd` con `sh` en `dir`, con la salida de error junto a la normal,
/// guardada entera en `log`. Lo mata (con todo lo que lanzó) si pasa de
/// `timeout`. Devuelve si salió bien y las últimas líneas.
fn execute(
    dir: &Path,
    cmd: &str,
    log: &Path,
    timeout: Duration,
    echo: &mut impl FnMut(&str),
) -> Result<(bool, String)> {
    use std::os::unix::process::CommandExt;
    // Antes de lanzarlo: si falla después, el comando quedaría corriendo sin
    // nadie que lo vigile.
    let mut file = File::create(log)?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{cmd}"))
        .current_dir(dir)
        // Lo que lance (p. ej. tests que abren `claude`) no es el agente.
        .env_remove("PASTOR_AGENT")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("no pude ejecutar sh")?;
    let pid = child.id();
    let out = child.stdout.take().context("sin salida")?;
    // Leer la salida bloquea: va en su hilo, y el de aquí vigila el tiempo.
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        // Por bytes: una línea que no es UTF-8 (un nombre de archivo raro)
        // no debe cortar la lectura, o el comando moriría por SIGPIPE.
        let mut out = BufReader::new(out);
        let mut buf = Vec::new();
        while matches!(out.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            let line = String::from_utf8_lossy(&buf);
            let line = line.strip_suffix('\n').unwrap_or(&line);
            if tx
                .send(line.strip_suffix('\r').unwrap_or(line).to_string())
                .is_err()
            {
                break;
            }
            buf.clear();
        }
    });
    let mut tail: Vec<String> = Vec::new();
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            let _ = writeln!(file, "{line}");
            echo(&line);
            tail.push(line);
            if tail.len() > TAIL_LINES {
                tail.remove(0);
            }
        }
        // Al grupo entero: `sh` y lo que lanzó.
        let kill = |child: &mut std::process::Child| {
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .status();
            let _ = child.wait();
        };
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            // Sin poder vigilarlo, no se deja corriendo: es fallo de pastor.
            Err(e) => {
                kill(&mut child);
                return Err(e).context("no pude vigilar el comando");
            }
            Ok(None) if start.elapsed() > timeout => {
                timed_out = true;
                kill(&mut child);
                break None;
            }
            Ok(None) => {}
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // Lo que quedó en la tubería, un momento como mucho: si algo que lanzó
    // sigue vivo con ella abierta, no se cierra nunca.
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        let _ = writeln!(file, "{line}");
        echo(&line);
        tail.push(line);
    }
    drop(reader);
    let mut text = tail[tail.len().saturating_sub(TAIL_LINES)..].join("\n");
    if text.len() > TAIL_BYTES {
        let mut cut = text.len() - TAIL_BYTES;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = format!("…{}", &text[cut..]);
    }
    if timed_out {
        text.push_str(&format!(
            "\n(pastor la detuvo: pasó de {} minutos)",
            timeout.as_secs() / 60
        ));
    }
    Ok((status.is_some_and(|s| s.success()), text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_in(cmd: &str, timeout: Duration) -> (bool, String, String) {
        let dir = std::env::temp_dir().join(format!("pastor-check-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join(format!("{}.log", cmd.len()));
        let mut seen = Vec::new();
        let (ok, tail) = execute(&dir, cmd, &log, timeout, &mut |l: &str| {
            seen.push(l.to_string())
        })
        .unwrap();
        let full = fs::read_to_string(&log).unwrap();
        assert_eq!(
            full,
            seen.iter().map(|l| format!("{l}\n")).collect::<String>()
        );
        (ok, tail, full)
    }

    #[test]
    fn junta_la_salida_de_error_y_guarda_las_ultimas_lineas() {
        let (ok, tail, full) = run_in("echo uno; echo dos >&2; exit 3", TIMEOUT);
        assert!(!ok);
        assert_eq!(tail, "uno\ndos");
        assert_eq!(full, "uno\ndos\n");
        let (ok, tail, full) = run_in("seq 1 100", TIMEOUT);
        assert!(ok);
        assert_eq!(full.lines().count(), 100);
        assert_eq!(tail.lines().count(), TAIL_LINES);
        assert!(tail.ends_with("\n100"));
    }

    #[test]
    fn una_linea_que_no_es_utf8_no_corta_la_lectura() {
        let (ok, tail, _) = run_in("printf 'a\\377b\\n'; seq 1 3", TIMEOUT);
        assert!(ok);
        assert_eq!(tail, "a\u{fffd}b\n1\n2\n3");
    }

    #[test]
    fn detiene_lo_que_tarda_demasiado_con_lo_que_lanzo() {
        let start = Instant::now();
        // El `sleep` de fondo hereda la tubería: no debe dejarla colgada.
        let (ok, tail, _) = run_in(
            "echo antes; sleep 30 & sleep 30",
            Duration::from_millis(500),
        );
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(!ok);
        assert!(tail.starts_with("antes\n"), "{tail}");
        assert!(tail.contains("pastor la detuvo"));
    }
}
