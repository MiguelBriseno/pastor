//! Avisos de los agentes que terminan o te esperan: en el escritorio y, si
//! está configurado, en el celular con ntfy. Los manda un solo proceso a la
//! vez, el que tiene el candado `avisos.lock`: con la TUI y `pastor serve`
//! abiertos, si no, cada aviso llegaría dos veces. Si hay una TUI abierta
//! (lo dice `tui.lock`), avisa ella: mira los paneles de todos modos, y
//! `serve` puede correr sin sesión gráfica, donde el aviso de escritorio no
//! sale.

use std::collections::HashMap;
#[cfg(feature = "serve")]
use std::collections::HashSet;
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
    /// En la TUI: el candado compartido que dice a `serve` que está abierta.
    /// Solo se guarda para no soltarlo.
    _tui: Option<File>,
}

impl Notifier {
    /// El de la TUI, con los estados que ya se veían, para no perder un aviso.
    pub fn for_tui(status: HashMap<String, Status>) -> Self {
        Self {
            status,
            lock: None,
            _tui: open_lock(&store::data_dir(), "tui.lock").filter(|f| f.try_lock_shared().is_ok()),
        }
    }

    /// Suelta el candado de los avisos, para que los mande la TUI.
    #[cfg(feature = "serve")]
    fn step_down(&mut self) {
        self.lock = None;
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
        let events = transitions(state, &self.status, snaps);
        // Una pasada suena una sola vez, como en la app del celular: si alguno
        // te espera, gana ese.
        let mut sound = events
            .iter()
            .map(|e| e.0)
            .min_by_key(|&e| e != Event::Blocked);
        for (_, msg, finished) in events {
            if lead {
                notify(sound.take(), &msg);
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
    let file = open_lock(dir, "avisos.lock")?;
    file.try_lock().ok()?;
    Some(file)
}

fn open_lock(dir: &Path, name: &str) -> Option<File> {
    fs::create_dir_all(dir).ok()?;
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(name))
        .ok()
}

/// Si hay alguna TUI abierta: cada una tiene `tui.lock` compartido, y así no
/// se puede tomar entero.
#[cfg(feature = "serve")]
fn tui_open_in(dir: &Path) -> bool {
    open_lock(dir, "tui.lock").is_some_and(|f| f.try_lock().is_err())
}

#[cfg(feature = "serve")]
/// Para `pastor serve`: mientras no haya una TUI abierta (que avisa ella),
/// mira los paneles y avisa desde aquí. No vuelve nunca. Al tomar el relevo
/// parte de cero: lo que cambie en ese segundo no avisa.
pub fn watch_forever() {
    let mut notifier = Notifier::default();
    let mut tracker = Tracker::default();
    // Los errores de la pasada anterior: tmux caído fallaría en cada una, y
    // el registro solo debe tenerlos una vez.
    let mut logged = HashSet::new();
    loop {
        std::thread::sleep(REFRESH);
        // Con una TUI abierta avisa ella: se le deja el candado.
        if tui_open_in(&store::data_dir()) {
            notifier.step_down();
            notifier.forget();
            continue;
        }
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
        let mut now = HashSet::new();
        for e in errors {
            let text = format!("{e:#}");
            if !logged.contains(&text) {
                hooks::log_error("avisos", &e);
            }
            now.insert(text);
        }
        logged = now;
    }
}

/// De qué avisa: cada uno suena distinto.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Event {
    Blocked,
    Finished,
}

/// Avisos por los cambios de estado de `old` a `new`, cada uno con el agente
/// cuya tarea hay que pasar a revisión, si la tiene.
fn transitions(
    state: &State,
    old: &HashMap<String, Status>,
    new: &HashMap<String, Snapshot>,
) -> Vec<(Event, String, Option<String>)> {
    let mut events = Vec::new();
    for a in &state.agents {
        let (Some(&old), Some(new)) = (old.get(&a.id), new.get(&a.id)) else {
            continue;
        };
        match (old, new.status) {
            (o, Status::Blocked) if o != Status::Blocked => {
                events.push((
                    Event::Blocked,
                    format!("{} necesita tu atención", a.name),
                    None,
                ));
            }
            (Status::Working, Status::Idle | Status::Dead) => {
                // Sin tarea no hay nada que pasar a revisión.
                let id = a.task.is_some().then(|| a.id.clone());
                events.push((Event::Finished, format!("{} terminó", a.name), id));
            }
            _ => {}
        }
    }
    events
}

/// Un aviso para el hilo que los muestra: con qué suena (o nada) y qué dice.
type Notice = (Option<Event>, String);

/// Muestra un aviso del sistema. Los lanza, de uno en uno, un único hilo que
/// espera a cada proceso (para que no quede zombi) sin frenar al observador.
fn notify(sound: Option<Event>, msg: &str) {
    static QUEUE: Mutex<Option<Sender<Notice>>> = Mutex::new(None);
    let mut queue = QUEUE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut notice = (sound, msg.to_string());
    if let Some(tx) = queue.as_ref() {
        match tx.send(notice) {
            Ok(()) => return,
            Err(e) => notice = e.0,
        }
    }
    // Sin hilo todavía, o murió: se lanza otro. Si no se puede, se intentará
    // con el próximo aviso.
    let (tx, rx) = mpsc::channel::<Notice>();
    let spawned = std::thread::Builder::new()
        .name("avisos".into())
        .spawn(move || {
            for (sound, msg) in rx {
                show_notice(sound, &msg);
            }
        });
    if spawned.is_ok() && tx.send(notice).is_ok() {
        *queue = Some(tx);
    }
}

/// Cuánto puede tardar un aviso antes de darlo por colgado (notify-send
/// espera hasta 25 s si nadie atiende las notificaciones).
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

fn show_notice(sound: Option<Event>, msg: &str) {
    // Todos a la vez, para que uno colgado no retrase a los demás.
    let desktop = notify_command(msg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let phone = ntfy::publish(msg);
    // Mientras suena, los otros dos ya van corriendo.
    if let Some(event) = sound {
        play(event);
    }
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

/// Hace sonar el aviso, salvo con `PASTOR_SOUND=0`. Si un reproductor falla
/// (`pw-play` instalado con PulseAudio de servidor), prueba el siguiente;
/// si se colgó no, que pudo haber sonado y sonaría dos veces.
fn play(event: Event) {
    if std::env::var("PASTOR_SOUND").as_deref() == Ok("0") {
        return;
    }
    for mut cmd in sound_commands(event) {
        let spawned = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(child) = spawned else { continue };
        match wait_or_kill(child, NOTICE_TIMEOUT) {
            Some(st) if !st.success() => continue,
            _ => return,
        }
    }
}

#[cfg(target_os = "macos")]
fn sound_commands(event: Event) -> Vec<Command> {
    let name = match event {
        Event::Blocked => "Ping",
        Event::Finished => "Glass",
    };
    let mut cmd = Command::new("afplay");
    cmd.arg(format!("/System/Library/Sounds/{name}.aiff"));
    vec![cmd]
}

/// Los sonidos de freedesktop, con PipeWire o, si no está, PulseAudio.
#[cfg(not(target_os = "macos"))]
fn sound_commands(event: Event) -> Vec<Command> {
    let name = match event {
        Event::Blocked => "message-new-instant",
        Event::Finished => "complete",
    };
    let file = format!("/usr/share/sounds/freedesktop/stereo/{name}.oga");
    ["pw-play", "paplay"]
        .into_iter()
        .map(|player| {
            let mut cmd = Command::new(player);
            cmd.arg(&file);
            cmd
        })
        .collect()
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

    #[cfg(feature = "serve")]
    #[test]
    fn serve_sabe_si_hay_una_tui_abierta() {
        let dir = std::env::temp_dir().join(format!("pastor-tui-{}", std::process::id()));
        assert!(!tui_open_in(&dir));
        let take = || {
            let f = open_lock(&dir, "tui.lock").unwrap();
            f.try_lock_shared().unwrap();
            f
        };
        // Dos TUI a la vez: hasta que salen las dos, sigue habiendo una.
        let (a, b) = (take(), take());
        assert!(tui_open_in(&dir));
        drop(a);
        assert!(tui_open_in(&dir));
        drop(b);
        let start = Instant::now();
        while tui_open_in(&dir) {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "el candado no se soltó"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(&dir);
    }

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
                (
                    Event::Finished,
                    format!("{} terminó", a1.name),
                    Some("a1".to_string())
                ),
                (Event::Blocked, "api necesita tu atención".to_string(), None),
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
        assert_eq!(transitions(&state, &old, &new)[0].2, None);
    }
}
