//! Estado persistente: agentes y tareas en ~/.local/share/pastor/state.json.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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
    /// Si trabaja en su propio worktree de git, cuál.
    #[serde(default)]
    pub worktree: Option<Worktree>,
    /// La sesión de Claude Code en la que va (la reportan sus hooks), para
    /// retomar la conversación si se pierde su sesión de tmux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Un worktree de git que pastor creó para un agente: su propia copia del
/// repositorio en su propia rama.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// El repositorio de donde salió, donde se integra la rama al aprobar.
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// El commit del que salió la rama: lo que hay que revisar es lo que
    /// cambió desde aquí.
    pub base: String,
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
    /// El worktree donde se hizo la tarea, para revisarla e integrarla.
    #[serde(default)]
    pub worktree: Option<Worktree>,
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
        Self::update_then(f, |_| {})
    }

    /// Lee con el candado tomado: nadie añade ni quita agentes mientras `f`
    /// corre.
    pub fn with_lock<T>(f: impl FnOnce(&State) -> T) -> Result<T> {
        ensure_dirs()?;
        let dir = data_dir();
        let _lock = lock(&dir.join("state.lock"))?;
        Ok(f(&Self::load_in(&dir)?))
    }

    /// Como `update`, y si se guardó, `saved` corre todavía con el candado
    /// tomado: lo que anote queda en el mismo orden que los guardados.
    pub fn update_then<T>(
        f: impl FnOnce(&mut State) -> Result<T>,
        saved: impl FnOnce(&mut T),
    ) -> Result<T> {
        ensure_dirs()?;
        Self::update_in(&data_dir(), f, saved)
    }

    fn load_in(dir: &Path) -> Result<Self> {
        match fs::read_to_string(dir.join("state.json")) {
            Ok(s) => serde_json::from_str(&s).with_context(|| {
                format!(
                    "state.json está corrupto; la última versión buena está en {}",
                    dir.join("state.json.bak").display()
                )
            }),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn update_in<T>(
        dir: &Path,
        f: impl FnOnce(&mut State) -> Result<T>,
        saved: impl FnOnce(&mut T),
    ) -> Result<T> {
        let _lock = lock(&dir.join("state.lock"))?;
        let mut s = Self::load_in(dir)?;
        let mut out = f(&mut s)?;
        let path = dir.join("state.json");
        let new = serde_json::to_string_pretty(&s)?;
        // Sin cambios no se toca nada: un enlace sin `rename` detrás dejaría
        // `state.json.bak` y `state.json` en el mismo inodo, y editar uno a
        // mano rompería los dos.
        if fs::read(&path).ok().as_deref() != Some(new.as_bytes()) {
            // El archivo que se va a reemplazar se pudo leer: queda como
            // respaldo. Con un enlace no se copia nada, y el `rename` deja el
            // contenido viejo en `state.json.bak`.
            let bak = dir.join("state.json.bak");
            let _ = fs::remove_file(&bak);
            if fs::hard_link(&path, &bak).is_err() {
                // Sistemas de archivos sin enlaces duros.
                let _ = fs::copy(&path, &bak);
            }
            replace(&path, new.as_bytes(), None, true)?;
        }
        saved(&mut out);
        Ok(out)
    }

    /// Cuántos agentes y tareas se han creado en total.
    pub fn counters(&self) -> (u32, u32) {
        (self.next_agent, self.next_task)
    }

    /// Que el próximo id de agente no sea uno que `taken` da por ocupado.
    pub fn skip_ids(&mut self, taken: impl Fn(&str) -> bool) {
        while taken(&format!("a{}", self.next_agent + 1)) {
            self.next_agent += 1;
        }
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
        // Un nombre como `a3` se confundiría con el id de otro agente.
        let base = if looks_like_id(&base) {
            format!("{base}-agente")
        } else {
            base
        };
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
            worktree: None,
            session: None,
        };
        self.agents.push(agent.clone());
        agent
    }

    /// Por id y, si no hay ninguno con ese id, por nombre.
    pub fn find_agent(&self, key: &str) -> Result<&Agent> {
        self.agents
            .iter()
            .find(|a| a.id == key)
            .or_else(|| self.agents.iter().find(|a| a.name == key))
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
            worktree: None,
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

/// Si `s` tiene la forma de un id de agente (`a1`, `a27`…).
/// Candado exclusivo sobre `path`, que dura lo que el `File` devuelto.
pub fn lock(path: &Path) -> Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("no pude abrir {}", path.display()))?;
    match file.lock() {
        // Algunos montajes (NFS, FUSE) no tienen candados: mejor seguir
        // como antes que dejar de funcionar.
        Err(e) if e.kind() == ErrorKind::Unsupported => {}
        r => r.with_context(|| format!("no pude bloquear {}", path.display()))?,
    }
    Ok(file)
}

pub fn looks_like_id(s: &str) -> bool {
    s.strip_prefix('a')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Sufijo distinto en cada llamada, también entre procesos y hilos, para
/// nombres temporales.
pub fn unique_suffix() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

/// Reemplaza `path` de golpe: escribe un temporal propio, lo lleva a disco y
/// lo renombra. Quien lo lea a la vez ve el archivo viejo o el nuevo, nunca
/// uno a medias, y un corte de luz no lo deja vacío.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    write_atomic_mode(path, contents, None)
}

/// Como `write_atomic`, con permisos `mode` (p. ej. 0o755 para un script).
/// Si el archivo ya tiene ese contenido no lo reescribe.
pub fn write_atomic_mode(path: &Path, contents: &[u8], mode: Option<u32>) -> Result<()> {
    if fs::read(path).is_ok_and(|old| old == contents) {
        if let Some(m) = mode {
            fs::set_permissions(path, fs::Permissions::from_mode(m))?;
        }
        return Ok(());
    }
    replace(path, contents, mode, true)
}

/// Como `write_atomic`, sin llevarlo a disco ni comparar: para archivos que
/// cambian a menudo y se pueden perder (el estado que reportan los hooks).
pub fn write_volatile(path: &Path, contents: &[u8]) -> Result<()> {
    replace(path, contents, None, false)
}

fn replace(path: &Path, contents: &[u8], mode: Option<u32>, durable: bool) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", unique_suffix()));
    let tmp = PathBuf::from(tmp);
    let written = (|| -> Result<()> {
        // Con el modo desde que se crea: un secreto no pasa por 0644 ni un
        // instante.
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode.unwrap_or(0o666))
                .open(&tmp)?
        };
        f.write_all(contents)?;
        if let Some(m) = mode {
            f.set_permissions(fs::Permissions::from_mode(m))?;
        }
        if durable {
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        // El `rename` vive en el directorio: sin esto, tras un corte de luz
        // puede volver el archivo viejo. Sin `?`: el archivo ya está en su
        // sitio, y algunos sistemas de archivos no dejan sincronizar un
        // directorio; un error aquí haría creer que no se escribió.
        if durable {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = File::open(dir).and_then(|d| d.sync_all());
            }
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("no pude escribir {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reusa_el_id_de_una_sesion_viva() {
        let mut s = State::default();
        s.skip_ids(|id| id == "a1" || id == "a2");
        assert_eq!(
            s.add_agent(Kind::Claude, PathBuf::from("/x"), None).id,
            "a3"
        );
    }

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

    /// Un agente llamado como el id de otro no lo tapa.
    #[test]
    fn un_nombre_no_tapa_un_id() {
        let mut s = State::default();
        let a = s.add_agent(Kind::Claude, PathBuf::from("/home/yo/a2"), None);
        assert_eq!(a.name, "a2-agente");
        let a = s.add_agent(Kind::Claude, PathBuf::from("/"), Some("a1".into()));
        assert_eq!((a.id.as_str(), a.name.as_str()), ("a2", "a1-agente"));
        // Aunque venga de un state.json viejo, el id gana.
        s.agents[0].name = "a2".into();
        assert_eq!(s.find_agent("a2").unwrap().id, "a2");
        assert_eq!(s.find_agent("a1").unwrap().id, "a1");
        assert!(!looks_like_id("a") && !looks_like_id("ab1") && looks_like_id("a10"));
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
                        State::update_in(
                            &dir,
                            |s| Ok(s.add_task(format!("{h}-{i}"), String::new())),
                            |_| {},
                        )
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

    /// Si `state.json` se corrompe, el error nombra el respaldo y este
    /// tiene la versión anterior al último guardado.
    #[test]
    fn respaldo_de_state_json() {
        let dir = std::env::temp_dir().join(format!(
            "pastor-test-bak-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        let add =
            |t: &str| State::update_in(&dir, |s| Ok(s.add_task(t.into(), String::new())), |_| {});
        add("uno").unwrap();
        add("dos").unwrap();
        // Una pasada sin cambios no deja `state.json` enlazado al respaldo:
        // si no, la escritura de abajo rompería los dos.
        State::update_in(&dir, |_| Ok(()), |_| {}).unwrap();
        let links = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(dir.join("state.json")).unwrap().nlink()
        };
        assert_eq!(links, 1);
        fs::write(dir.join("state.json"), "{roto").unwrap();
        let err = add("tres").map(|_| ()).unwrap_err().to_string();
        let bak: Result<State> = fs::read_to_string(dir.join("state.json.bak"))
            .map_err(Into::into)
            .and_then(|b| serde_json::from_str(&b).map_err(Into::into));
        let _ = fs::remove_dir_all(&dir);
        assert!(err.contains("state.json.bak"), "{err}");
        assert_eq!(bak.unwrap().tasks.len(), 1);
    }
}
