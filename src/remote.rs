//! Agentes en otras máquinas. Cada máquina tiene su propio pastor, dueño de
//! sus agentes (su tmux, sus hooks, su estado); este le habla por SSH con
//! `ssh <destino> pastor …` y muestra sus agentes junto a los de aquí, con el
//! nombre de la máquina delante del id: `servidor:a3`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::Status;
use crate::store::{self, Agent};

/// Las máquinas registradas: su nombre en pastor y el destino de SSH
/// (`usuario@host` o un alias de `~/.ssh/config`).
pub type Hosts = BTreeMap<String, String>;

fn hosts_path() -> PathBuf {
    store::data_dir().join("hosts.json")
}

pub fn hosts() -> Result<Hosts> {
    match fs::read_to_string(hosts_path()) {
        Ok(s) => serde_json::from_str(&s).context("hosts.json está corrupto"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Hosts::new()),
        Err(e) => Err(e.into()),
    }
}

fn save(hosts: &Hosts) -> Result<()> {
    store::ensure_dirs()?;
    store::write_atomic(
        &hosts_path(),
        serde_json::to_string_pretty(hosts)?.as_bytes(),
    )
}

pub fn add_host(name: &str, dest: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("el nombre de la máquina solo puede tener letras, números, - y _");
    }
    if store::looks_like_id(name) {
        bail!("`{name}` parece el id de un agente; elige otro nombre");
    }
    // Empezando por `-`, ssh lo tomaría como una opción.
    if dest.is_empty() || dest.starts_with('-') || dest.contains(char::is_whitespace) {
        bail!("`{dest}` no es un destino de SSH (p. ej. usuario@host)");
    }
    let mut hosts = hosts()?;
    hosts.insert(name.into(), dest.into());
    save(&hosts)
}

pub fn remove_host(name: &str) -> Result<()> {
    let mut hosts = hosts()?;
    if hosts.remove(name).is_none() {
        bail!("no existe la máquina `{name}`");
    }
    save(&hosts)
}

/// Si `key` es de un agente de otra máquina (`servidor:a3`), la máquina y lo
/// que hay que buscar allá.
pub fn split(key: &str) -> Option<(String, &str)> {
    let (host, rest) = key.split_once(':')?;
    if rest.is_empty() {
        return None;
    }
    hosts()
        .ok()?
        .contains_key(host)
        .then(|| (host.to_string(), rest))
}

/// `ssh` a la máquina, con una conexión compartida por máquina que dura unos
/// minutos: la TUI pregunta cada segundo y abrir una cada vez tarda. Sin
/// preguntar nada (`BatchMode`): sin llave, falla en vez de quedarse
/// esperando una contraseña que nadie va a escribir. Con `tty`, con
/// terminal para entrar a un agente. Las opciones van antes del destino: lo
/// que sigue a él es el comando de allá.
fn ssh(host: &str, tty: bool) -> Result<Command> {
    let dest = hosts()?
        .remove(host)
        .ok_or_else(|| anyhow!("no existe la máquina `{host}`"))?;
    let sockets = store::data_dir().join("ssh");
    fs::create_dir_all(&sockets)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700))?;
    }
    let mut c = Command::new("ssh");
    for opt in [
        "BatchMode=yes".to_string(),
        "ConnectTimeout=5".into(),
        "ServerAliveInterval=5".into(),
        "ServerAliveCountMax=2".into(),
        "ControlMaster=auto".into(),
        // `%C` es un hash de la conexión: corto, para no pasar del límite de
        // las rutas de un socket.
        format!("ControlPath={}/%C", sockets.display()),
        "ControlPersist=10m".into(),
    ] {
        c.arg("-o").arg(opt);
    }
    if tty {
        c.arg("-t");
    }
    c.arg(dest);
    Ok(c)
}

/// Lo que se le pide al shell de la otra máquina: un `sh` que encuentre a
/// pastor aunque el shell sin login no tenga `~/.cargo/bin` en el PATH, y le
/// pase los argumentos en hexadecimal. El shell de allá puede ser cualquiera
/// (fish cita distinto que sh), y así ninguno los interpreta: solo hay
/// letras y números.
fn remote_command(args: &[&str]) -> Vec<String> {
    let mut cmd = vec![
        "sh".to_string(),
        "-c".into(),
        "'PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$PATH\"; export PATH; exec pastor \"$@\"'"
            .into(),
        "pastor".into(),
        HEX_ARGS.into(),
    ];
    cmd.extend(args.iter().map(|a| hex(a.as_bytes())));
    cmd
}

/// El primer argumento con el que llegan los argumentos en hexadecimal.
pub const HEX_ARGS: &str = "_hex";

fn hex(bytes: &[u8]) -> String {
    // Vacío no se puede: el shell de allá se lo comería. Un `x` delante
    // nunca es hexadecimal.
    let mut s = String::from("x");
    s.extend(bytes.iter().map(|b| format!("{b:02x}")));
    s
}

/// Al revés que `hex`: los argumentos que llegaron de otra máquina.
pub fn unhex(arg: &str) -> Result<String> {
    let digits = arg
        .strip_prefix('x')
        .ok_or_else(|| anyhow!("argumento mal codificado"))?;
    if digits.len() % 2 != 0 {
        bail!("argumento mal codificado");
    }
    let bytes = (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .context("argumento mal codificado")?;
    String::from_utf8(bytes).context("argumento mal codificado")
}

/// Corre `pastor <args>` en la otra máquina y devuelve lo que imprimió. Si
/// falla, el error es el de allá.
pub fn run(host: &str, args: &[&str]) -> Result<String> {
    let out = ssh(host, false)?
        .args(remote_command(args))
        .stdin(Stdio::null())
        .output()
        .context("no pude ejecutar ssh")?;
    answer(host, out)
}

/// Como `run`, mandando `input` por la entrada del pastor de allá (lo que
/// no cabe en un argumento, como un archivo entero).
pub fn run_input(host: &str, args: &[&str], input: &[u8]) -> Result<String> {
    use std::io::Write;
    let mut child = ssh(host, false)?
        .args(remote_command(args))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("no pude ejecutar ssh")?;
    // Allá se lee la entrada entera antes de responder: escribirla toda
    // primero no se traba. Si ssh no conectó, el error es el de su salida.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let out = child.wait_with_output().context("no pude ejecutar ssh")?;
    answer(host, out)
}

/// Lo que imprimió el pastor de allá o, si falló, su error.
fn answer(host: &str, out: std::process::Output) -> Result<String> {
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    // 255 es de ssh: no se pudo conectar. Lo demás lo dijo pastor allá.
    if out.status.code() == Some(255) {
        bail!("no pude conectar con {host}: {err}");
    }
    if err.contains("pastor: not found") || err.contains("pastor: command not found") {
        bail!("{host} no tiene pastor instalado (cargo install --path . allá)");
    }
    bail!("{host}: {}", err.strip_prefix("Error: ").unwrap_or(err));
}

/// Como `run`, con la terminal de aquí: para entrar a un agente.
pub fn interactive(host: &str, args: &[&str]) -> Result<()> {
    let status = ssh(host, true)?
        .args(remote_command(args))
        .status()
        .context("no pude ejecutar ssh")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(255) => bail!("no pude conectar con {host}"),
        _ => bail!("pastor falló en {host}"),
    }
}

/// Los agentes de una máquina, con su estado.
pub type Listing = Vec<(Agent, Status)>;

/// Un agente como lo lista `pastor ls --json`.
#[derive(Serialize, Deserialize)]
pub struct Listed {
    #[serde(flatten)]
    pub agent: Agent,
    pub status: String,
    #[serde(default)]
    pub status_label: String,
}

/// Los agentes de la otra máquina, con su id y su máquina ya puestos como
/// se ven aquí.
pub fn agents(host: &str) -> Result<Listing> {
    let out = run(host, &["ls", "--json"])?;
    let listed: Vec<Listed> = serde_json::from_str(&out)
        .with_context(|| format!("no entendí la lista de {host}; ¿su pastor es más viejo?"))?;
    Ok(listed
        .into_iter()
        .map(|l| {
            let mut a = l.agent;
            a.id = format!("{host}:{}", a.id);
            a.host = Some(host.to_string());
            (a, Status::from_key(&l.status).unwrap_or(Status::Dead))
        })
        .collect())
}

/// Los agentes de todas las máquinas, a la vez: una que no responde tarda
/// hasta que se rinde ssh, y no debe hacer esperar a las demás.
pub fn all_agents(hosts: &Hosts) -> Vec<(String, Result<Listing>)> {
    std::thread::scope(|scope| {
        let asked: Vec<_> = hosts
            .keys()
            .map(|h| (h, scope.spawn(move || agents(h))))
            .collect();
        asked
            .into_iter()
            .map(|(h, t)| {
                let result = t
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("falló al preguntarle")));
                (h.clone(), result)
            })
            .collect()
    })
}

/// Lo último que se supo de las otras máquinas.
#[derive(Default)]
pub struct Seen {
    /// Sus agentes o, si no respondió, por qué.
    pub hosts: BTreeMap<String, std::result::Result<Listing, String>>,
    /// La pantalla del agente remoto que se está mirando.
    pub screens: HashMap<String, String>,
}

/// Lo que comparten la TUI y los hilos que preguntan a las máquinas.
#[derive(Default)]
struct Shared {
    seen: Mutex<Seen>,
    focus: Mutex<Option<String>>,
    /// Las máquinas que ya tienen su hilo.
    running: Mutex<HashSet<String>>,
    /// Sube cada vez que hay que preguntar ya; con `cv` despierta a todos.
    wakes: Mutex<u64>,
    cv: Condvar,
    stop: AtomicBool,
}

impl Shared {
    /// Espera `every`, o menos si piden preguntar ya o se cierra la TUI.
    fn wait(&self, every: Duration) {
        let n = self.wakes.lock().unwrap_or_else(PoisonError::into_inner);
        let start = *n;
        let _ = self.cv.wait_timeout_while(n, every, |n| {
            *n == start && !self.stop.load(Ordering::Relaxed)
        });
    }

    fn wake(&self) {
        *self.wakes.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.cv.notify_all();
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Pregunta a cada máquina por sus agentes cada `every`, aparte del hilo que
/// mira los paneles de aquí. Cada máquina va en su propio hilo y publica en
/// cuanto responde: una lenta o caída (hasta que ssh se rinde) no frena lo
/// local ni a las demás. De la pantalla solo pide la del agente que se está
/// mirando.
pub struct Poller {
    shared: Arc<Shared>,
}

impl Poller {
    pub fn spawn(every: Duration) -> Poller {
        let shared = Arc::new(Shared::default());
        let s = Arc::clone(&shared);
        // Si no se puede lanzar, simplemente no se ven las otras máquinas.
        let _ = std::thread::Builder::new()
            .name("maquinas".into())
            .spawn(move || {
                // Las máquinas que se registren después también entran.
                while !s.stop.load(Ordering::Relaxed) {
                    for host in hosts().unwrap_or_default().into_keys() {
                        let new = s
                            .running
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(host.clone());
                        if new {
                            let spawned = std::thread::Builder::new()
                                .name(format!("maquina {host}"))
                                .spawn({
                                    let (s, host) = (Arc::clone(&s), host.clone());
                                    move || watch_host(&s, &host, every)
                                });
                            if spawned.is_err() {
                                s.running
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .remove(&host);
                            }
                        }
                    }
                    s.wait(every);
                }
            });
        Poller { shared }
    }

    /// El agente de otra máquina que se está mirando, para traer su
    /// pantalla. Si cambió, pregunta ya.
    pub fn focus(&self, id: Option<&str>) {
        let mut focus = self
            .shared
            .focus
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if focus.as_deref() != id {
            *focus = id.map(String::from);
            drop(focus);
            self.shared.wake();
        }
    }

    pub fn wake(&self) {
        self.shared.wake();
    }

    pub fn seen(&self) -> MutexGuard<'_, Seen> {
        self.shared.seen()
    }
}

/// Al cerrar la TUI, que los hilos terminen.
impl Drop for Poller {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake();
    }
}

/// El hilo de una máquina: pregunta, publica y espera, hasta que se cierra la
/// TUI o la quitan de la lista.
fn watch_host(s: &Shared, host: &str, every: Duration) {
    let prefix = format!("{host}:");
    let forget = |s: &Shared| {
        let mut seen = s.seen();
        seen.hosts.remove(host);
        seen.screens.retain(|id, _| !id.starts_with(&prefix));
    };
    while !s.stop.load(Ordering::Relaxed) {
        if !hosts().is_ok_and(|h| h.contains_key(host)) {
            forget(s);
            break;
        }
        let focus = s
            .focus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let focus = focus.as_deref().and_then(|f| f.strip_prefix(&prefix));
        let agents = agents(host).map_err(|e| format!("{e:#}"));
        let screen = match focus {
            Some(id) if agents.is_ok() => run(host, &["screen", id])
                .ok()
                .map(|out| (format!("{prefix}{id}"), out.trim_end().to_string())),
            _ => None,
        };
        {
            let mut seen = s.seen();
            seen.hosts.insert(host.to_string(), agents);
            // Solo la del que se mira: las demás ya no se actualizan.
            seen.screens.retain(|id, _| !id.starts_with(&prefix));
            seen.screens.extend(screen);
        }
        s.wait(every);
    }
    s.running
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(host);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_maquina_no_se_guarda_y_el_estado_ida_y_vuelta() {
        let mut s = store::State::default();
        let mut a = s.add_agent(crate::agent::Kind::Claude, PathBuf::from("/x"), None);
        a.host = Some("servidor".into());
        let json = serde_json::to_string(&Listed {
            agent: a,
            status: Status::Blocked.key().into(),
            status_label: Status::Blocked.label().into(),
        })
        .unwrap();
        assert!(!json.contains("servidor"), "{json}");
        let back: Listed = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent.host, None);
        assert_eq!(Status::from_key(&back.status), Some(Status::Blocked));
        for st in Status::ALL {
            assert_eq!(Status::from_key(st.key()), Some(st));
        }
    }

    #[test]
    fn los_argumentos_van_en_hexadecimal() {
        for arg in [
            "",
            "a3",
            "it's \"$HOME\" `x` \\n ; rm -rf ~",
            "ñandú 🦀",
            "-p",
        ] {
            let h = hex(arg.as_bytes());
            assert!(h.chars().all(|c| c.is_ascii_alphanumeric()), "{h}");
            assert_eq!(unhex(&h).unwrap(), arg);
        }
        assert!(unhex("abc").is_err());
        assert!(unhex("xa").is_err());
        assert!(unhex("xzz").is_err());
    }

    #[test]
    fn el_comando_de_alla_no_tiene_nada_que_interpretar() {
        let cmd = remote_command(&["send", "a3", "'; echo pwned"]);
        // Salvo el script fijo, todo son letras y números.
        assert_eq!(cmd[..3], ["sh", "-c", cmd[2].as_str()]);
        for a in &cmd[3..] {
            assert!(
                a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{a}"
            );
        }
        // El script no tiene `\` ni `'` por dentro: fish lo lee como sh.
        let inner = &cmd[2][1..cmd[2].len() - 1];
        assert!(!inner.contains('\\') && !inner.contains('\''), "{inner}");
    }
}
