//! Avisos de los agentes que terminan o te esperan: en el escritorio y, si
//! está configurado, en el celular con ntfy. Los manda un solo proceso a la
//! vez, el que tiene el candado `avisos.lock`: con la TUI y `pastor serve`
//! abiertos, si no, cada aviso llegaría dos veces.

use std::collections::HashMap;
use std::fs::{self, File};
#[cfg(feature = "serve")]
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

#[cfg(feature = "serve")]
use anyhow::{anyhow, Result};

use crate::agent::Status;
#[cfg(feature = "serve")]
use crate::hooks;
use crate::ntfy;
#[cfg(feature = "serve")]
use crate::ops::Tracker;
use crate::ops::{self, Snapshot};
use crate::store::{self, State};

/// Cada cuánto mira los paneles `pastor serve` cuando avisa él.
#[cfg(feature = "serve")]
const REFRESH: Duration = Duration::from_secs(1);

/// Recuerda el estado de cada agente para avisar de los cambios. Solo manda
/// avisos (y pasa tareas a revisión) mientras tiene el candado.
#[derive(Default)]
pub struct Notifier {
    status: HashMap<String, Status>,
    lock: Option<File>,
}

impl Notifier {
    /// Con los estados que ya se veían, para no perder un aviso.
    pub fn new(status: HashMap<String, Status>) -> Self {
        Self { status, lock: None }
    }

    /// Si este proceso manda los avisos: si aún no, intenta tomar el candado
    /// (el que lo tenía pudo haber salido).
    pub fn leading(&mut self) -> bool {
        if self.lock.is_none() {
            self.lock = take_lock();
        }
        self.lock.is_some()
    }

    #[cfg(feature = "serve")]
    /// Olvida los estados vistos: tras un rato sin mirar, compararlos con los
    /// de ahora daría avisos de cosas que pasaron hace mucho.
    pub fn forget(&mut self) {
        self.status.clear();
    }

    /// Los avisos por lo que cambió desde la pasada anterior, y los errores
    /// al pasar tareas a revisión. Si este proceso tiene el candado, además
    /// los manda y pasa las tareas; si no, solo los devuelve (la TUI los
    /// muestra igual en su pie).
    pub fn pass(
        &mut self,
        state: &State,
        snaps: &HashMap<String, Snapshot>,
    ) -> (Vec<String>, Vec<anyhow::Error>) {
        let lead = self.leading();
        let (mut msgs, mut errors) = (Vec::new(), Vec::new());
        for (msg, finished) in transitions(state, &self.status, snaps) {
            if lead {
                notify(&msg);
                if let Some(Err(e)) = finished.map(|id| ops::finish_turn(&id)) {
                    errors.push(e);
                }
            }
            msgs.push(msg);
        }
        self.status = snaps.iter().map(|(id, s)| (id.clone(), s.status)).collect();
        (msgs, errors)
    }
}

/// El candado de los avisos, si nadie más lo tiene. Lo suelta el sistema al
/// cerrarse el archivo, también si el proceso muere.
fn take_lock() -> Option<File> {
    take_lock_in(&store::data_dir())
}

fn take_lock_in(dir: &Path) -> Option<File> {
    fs::create_dir_all(dir).ok()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("avisos.lock"))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

#[cfg(feature = "serve")]
/// Para `pastor serve`: mientras la TUI no esté abierta (no tenga el
/// candado), mira los paneles y avisa desde aquí. No vuelve nunca.
pub fn watch_forever() {
    let mut notifier = Notifier::default();
    let mut tracker = Tracker::default();
    // El último error anotado: tmux caído fallaría en cada pasada.
    let mut last_error = String::new();
    loop {
        std::thread::sleep(REFRESH);
        if !notifier.leading() {
            notifier.forget();
            continue;
        }
        let pass = panic::catch_unwind(AssertUnwindSafe(|| -> Result<Vec<anyhow::Error>> {
            let state = State::load()?;
            let snaps = ops::observe(&state, &mut tracker)?;
            Ok(notifier.pass(&state, &snaps).1)
        }));
        let errors = match pass {
            Ok(Ok(errors)) => errors,
            Ok(Err(e)) => vec![e],
            Err(_) => {
                // Lo visto pudo quedar a medias.
                notifier.forget();
                tracker = Tracker::default();
                vec![anyhow!("el observador de avisos entró en pánico")]
            }
        };
        if errors.is_empty() {
            last_error.clear();
        }
        for e in errors {
            let text = format!("{e:#}");
            if text != last_error {
                hooks::log_error("avisos", &e);
                last_error = text;
            }
        }
    }
}

/// Avisos por los cambios de estado de `old` a `new`, cada uno con el agente
/// cuya tarea hay que pasar a revisión, si la tiene.
fn transitions(
    state: &State,
    old: &HashMap<String, Status>,
    new: &HashMap<String, Snapshot>,
) -> Vec<(String, Option<String>)> {
    let mut events = Vec::new();
    for a in &state.agents {
        let (Some(&old), Some(new)) = (old.get(&a.id), new.get(&a.id)) else {
            continue;
        };
        match (old, new.status) {
            (o, Status::Blocked) if o != Status::Blocked => {
                events.push((format!("{} necesita tu atención", a.name), None));
            }
            (Status::Working, Status::Idle | Status::Dead) => {
                // Sin tarea no hay nada que pasar a revisión.
                let id = a.task.is_some().then(|| a.id.clone());
                events.push((format!("{} terminó", a.name), id));
            }
            _ => {}
        }
    }
    events
}

/// Muestra un aviso del sistema. Los lanza, de uno en uno, un único hilo que
/// espera a cada proceso (para que no quede zombi) sin frenar al observador.
fn notify(msg: &str) {
    static QUEUE: Mutex<Option<Sender<String>>> = Mutex::new(None);
    let mut queue = QUEUE.lock().unwrap_or_else(PoisonError::into_inner);
    let msg = match queue.as_ref().map(|tx| tx.send(msg.to_string())) {
        Some(Ok(())) => return,
        Some(Err(e)) => e.0,
        None => msg.to_string(),
    };
    // Sin hilo todavía, o murió: se lanza otro. Si no se puede, se intentará
    // con el próximo aviso.
    let (tx, rx) = mpsc::channel::<String>();
    let spawned = std::thread::Builder::new()
        .name("avisos".into())
        .spawn(move || {
            for msg in rx {
                show_notice(&msg);
            }
        });
    if spawned.is_ok() && tx.send(msg).is_ok() {
        *queue = Some(tx);
    }
}

/// Cuánto puede tardar un aviso antes de darlo por colgado (notify-send
/// espera hasta 25 s si nadie atiende las notificaciones).
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

fn show_notice(msg: &str) {
    // Los dos a la vez, para que uno colgado no retrase al otro.
    let desktop = notify_command(msg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let phone = ntfy::publish(msg);
    if let Ok(child) = desktop {
        wait_or_kill(child, NOTICE_TIMEOUT);
    }
    if let Some(mut child) = phone {
        // curl ya corta a los 10 s (`-m`); esto es por si acaso.
        let stderr = child.stderr.take();
        ntfy::check(stderr, wait_or_kill(child, NTFY_TIMEOUT));
    }
}

/// Lo que tarda como mucho un aviso a ntfy.
const NTFY_TIMEOUT: Duration = Duration::from_secs(12);

/// Espera a `child` hasta `timeout`; si se pasa, lo mata para que no retrase
/// a los siguientes. Cómo terminó, o `None` si hubo que matarlo.
fn wait_or_kill(mut child: Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if start.elapsed() <= timeout => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn notify_command(msg: &str) -> Command {
    // El mensaje va como argumento y no dentro del AppleScript, para no
    // tener que escaparlo. El título va antes: si el mensaje empieza con `-`
    // (el nombre del agente), osascript ya no lo toma por una opción.
    let mut cmd = Command::new("osascript");
    cmd.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        "pastor",
        msg,
    ]);
    cmd
}

#[cfg(not(target_os = "macos"))]
fn notify_command(msg: &str) -> Command {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "pastor", "--", "pastor", msg]);
    cmd
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::Kind;

    #[test]
    fn avisa_un_solo_proceso() {
        let dir = std::env::temp_dir().join(format!("pastor-avisos-{}", std::process::id()));
        let first = take_lock_in(&dir).expect("el primero toma el candado");
        // Otro que lo intente (la TUI con serve ya abierto) se queda sin él…
        assert!(take_lock_in(&dir).is_none());
        // …hasta que el primero sale. Con reintentos: un hijo que otro test
        // lanza en ese momento comparte el candado hasta su `exec`.
        drop(first);
        let start = Instant::now();
        while take_lock_in(&dir).is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "el candado no se soltó"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn snaps(list: &[(&str, Status)]) -> HashMap<String, Snapshot> {
        list.iter()
            .map(|&(id, status)| {
                let snap = Snapshot {
                    status,
                    screen: String::new(),
                    term: None,
                };
                (id.to_string(), snap)
            })
            .collect()
    }

    fn statuses(list: &[(&str, Status)]) -> HashMap<String, Status> {
        list.iter().map(|&(id, s)| (id.to_string(), s)).collect()
    }

    #[test]
    fn un_aviso_colgado_se_mata() {
        let spawn = |secs: &str| Command::new("sleep").arg(secs).spawn().unwrap();
        assert!(wait_or_kill(spawn("0"), Duration::from_secs(5)).is_some());
        let start = Instant::now();
        assert!(wait_or_kill(spawn("30"), Duration::from_millis(200)).is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn avisa_al_terminar_y_al_bloquearse() {
        let mut state = State::default();
        let a1 = state.add_agent(Kind::Claude, PathBuf::from("/tmp/web"), None);
        state.add_agent(Kind::Claude, PathBuf::from("/tmp/api"), None);
        state.agents[0].task = Some("t1".into());
        let old = statuses(&[("a1", Status::Working), ("a2", Status::Idle)]);
        let new = snaps(&[("a1", Status::Idle), ("a2", Status::Blocked)]);
        assert_eq!(
            transitions(&state, &old, &new),
            [
                (format!("{} terminó", a1.name), Some("a1".to_string())),
                ("api necesita tu atención".to_string(), None),
            ]
        );
        // Sin estado anterior (recién arrancado) no hay avisos.
        assert!(transitions(&state, &HashMap::new(), &new).is_empty());
    }

    #[test]
    fn sin_tarea_no_hay_nada_que_pasar_a_revision() {
        let mut state = State::default();
        state.add_agent(Kind::Claude, PathBuf::from("/tmp/web"), None);
        let old = statuses(&[("a1", Status::Working)]);
        let new = snaps(&[("a1", Status::Dead)]);
        assert_eq!(transitions(&state, &old, &new)[0].1, None);
    }
}
