//! Los archivos del directorio de un agente, para el editor: listarlos con lo
//! que cambió en git, leerlos y guardarlos. Los de un agente de otra máquina
//! se piden por SSH a su pastor (`pastor files`), que los lee allá.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::remote;
use crate::store::{Agent, State, Worktree};
use crate::worktree;

/// Cuántos archivos se listan como mucho: más no se recorren a mano.
pub const MAX_FILES: usize = 20_000;
/// El archivo más grande que se abre.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Cómo cambió un archivo en git.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Modified,
    Added,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// Relativa al directorio, con `/`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<Change>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Listing {
    pub root: PathBuf,
    pub files: Vec<Entry>,
    /// Si había más de `MAX_FILES`.
    #[serde(default)]
    pub truncated: bool,
}

/// Un archivo abierto. `hash` es el de su contenido al leerlo: al guardar, si
/// ya no coincide, alguien más (el agente) lo cambió mientras.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Opened {
    pub path: String,
    pub text: String,
    pub hash: String,
}

/// Dónde están los archivos.
#[derive(Clone, Debug)]
pub enum Source {
    /// Un directorio de esta máquina. Con el worktree del agente, lo que
    /// cambió se cuenta desde que salió su rama.
    Local {
        root: PathBuf,
        worktree: Option<Worktree>,
    },
    /// El agente `id` de la máquina `host`.
    Remote { host: String, id: String },
}

impl Source {
    /// El directorio del agente `key` (id o nombre, también `servidor:a3`).
    pub fn agent(key: &str) -> Result<Source> {
        if let Some((host, id)) = remote::split(key) {
            return Ok(Source::Remote {
                host,
                id: id.to_string(),
            });
        }
        let state = State::load()?;
        let a = state.find_agent(key)?;
        Ok(Source::Local {
            root: a.cwd.clone(),
            worktree: a.worktree.clone(),
        })
    }

    /// El directorio de `a`, que puede ser de otra máquina.
    pub fn of(a: &Agent) -> Source {
        match &a.host {
            Some(host) => Source::Remote {
                host: host.clone(),
                id: a
                    .id
                    .strip_prefix(&format!("{host}:"))
                    .unwrap_or(&a.id)
                    .to_string(),
            },
            None => Source::Local {
                root: a.cwd.clone(),
                worktree: a.worktree.clone(),
            },
        }
    }

    pub fn dir(root: PathBuf) -> Source {
        Source::Local {
            root,
            worktree: None,
        }
    }

    pub fn list(&self) -> Result<Listing> {
        match self {
            Source::Local { root, worktree } => list(root, worktree.as_ref()),
            Source::Remote { host, id } => {
                let out = remote::run(host, &["files", id])?;
                parse(host, &out)
            }
        }
    }

    pub fn read(&self, path: &str) -> Result<Opened> {
        match self {
            Source::Local { root, .. } => read(root, path),
            Source::Remote { host, id } => {
                let out = remote::run(host, &["files", id, "--read", path])?;
                parse(host, &out)
            }
        }
    }

    /// Guarda `text` en `path` si sigue como cuando se leyó (`hash`).
    /// Devuelve el hash nuevo.
    pub fn write(&self, path: &str, text: &str, hash: &str) -> Result<String> {
        match self {
            Source::Local { root, .. } => write(root, path, text, hash),
            Source::Remote { host, id } => {
                // El texto va por la entrada: como argumento tiene un límite.
                let out = remote::run_input(
                    host,
                    &["files", id, "--write", path, "--hash", hash],
                    text.as_bytes(),
                )?;
                let saved: Saved = parse(host, &out)?;
                Ok(saved.hash)
            }
        }
    }
}

/// Lo que responde `pastor files --write`.
#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub hash: String,
}

fn parse<T: serde::de::DeserializeOwned>(host: &str, out: &str) -> Result<T> {
    serde_json::from_str(out)
        .with_context(|| format!("no entendí la respuesta de {host}; ¿su pastor es más viejo?"))
}

/// FNV-1a, como la huella de las pantallas: estable entre versiones, y quien
/// lee y quien guarda pueden ser los pastor de dos máquinas.
pub fn hash(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    format!("{h:016x}")
}

pub fn list(root: &Path, wt: Option<&Worktree>) -> Result<Listing> {
    if !root.is_dir() {
        bail!("el directorio {} no existe", root.display());
    }
    let repo = worktree::toplevel(root).is_some();
    let mut files = Vec::new();
    if repo {
        files = worktree::files(root, MAX_FILES + 1)?;
    } else {
        worktree::walk(root, Path::new(""), MAX_FILES + 1, &mut files);
    }
    files.sort();
    files.dedup();
    let truncated = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    let changes: HashMap<String, Change> = if repo {
        worktree::changes(root, wt)
            .unwrap_or_default()
            .into_iter()
            .map(|(p, new)| (p, if new { Change::Added } else { Change::Modified }))
            .collect()
    } else {
        HashMap::new()
    };
    Ok(Listing {
        root: root.to_path_buf(),
        files: files
            .into_iter()
            .map(|path| Entry {
                change: changes.get(&path).copied(),
                path,
            })
            .collect(),
        truncated,
    })
}

/// La ruta completa de `rel`, que debe ser un archivo dentro de `root`. Un
/// enlace que lleve fuera tampoco vale: desde el celular, `../` o un enlace
/// a `/etc` abrirían cualquier cosa.
fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    let path = Path::new(rel);
    let plain = path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if rel.is_empty() || !plain {
        bail!("`{rel}` no es una ruta dentro del directorio");
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("el directorio {} no existe", root.display()))?;
    let full = root
        .join(path)
        .canonicalize()
        .map_err(|_| anyhow!("no existe el archivo {rel}"))?;
    if !full.starts_with(&root) {
        bail!("{rel} lleva fuera del directorio");
    }
    if !full.is_file() {
        bail!("{rel} no es un archivo");
    }
    Ok(full)
}

/// El contenido de `full`, si es texto y no es demasiado grande.
fn contents(full: &Path, rel: &str) -> Result<Vec<u8>> {
    let size = fs::metadata(full)?.len();
    if size > MAX_BYTES {
        bail!(
            "{rel} pesa {:.1} MB; el editor abre hasta {} MB",
            size as f64 / 1e6,
            MAX_BYTES / (1024 * 1024)
        );
    }
    let bytes = fs::read(full).with_context(|| format!("no pude leer {rel}"))?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        bail!("{rel} no es texto (o no está en UTF-8)");
    }
    Ok(bytes)
}

pub fn read(root: &Path, rel: &str) -> Result<Opened> {
    let full = resolve(root, rel)?;
    let bytes = contents(&full, rel)?;
    Ok(Opened {
        path: rel.to_string(),
        hash: hash(&bytes),
        text: String::from_utf8(bytes)?,
    })
}

pub fn write(root: &Path, rel: &str, text: &str, expected: &str) -> Result<String> {
    let full = resolve(root, rel)?;
    if text.len() as u64 > MAX_BYTES {
        bail!("el texto pesa más de {} MB", MAX_BYTES / (1024 * 1024));
    }
    // Si creció de más desde que se abrió, ya no es el que se leyó.
    if fs::metadata(&full)?.len() > MAX_BYTES {
        bail!("{rel} cambió desde que lo abriste (¿lo tocó el agente?); vuelve a abrirlo");
    }
    // Entre comparar y escribir queda una ventana mínima en la que el agente
    // podría escribir; sin bloqueos que él respete no se puede cerrar.
    let now = fs::read(&full).with_context(|| format!("no pude leer {rel}"))?;
    if hash(&now) != expected {
        bail!("{rel} cambió desde que lo abriste (¿lo tocó el agente?); vuelve a abrirlo");
    }
    // Encima del mismo archivo, sin renombrar: se quedan sus permisos (un
    // `.env` con 600) y sus enlaces.
    fs::write(&full, text).with_context(|| format!("no pude guardar {rel}"))?;
    Ok(hash(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("pastor-files-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Dir(d)
        }

        fn put(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }

        fn git(&self, args: &[&str]) {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths(l: &Listing) -> Vec<(&str, Option<Change>)> {
        l.files
            .iter()
            .map(|e| (e.path.as_str(), e.change))
            .collect()
    }

    #[test]
    fn sin_git_recorre_la_carpeta_sin_las_pesadas() {
        let d = Dir::new("sin-git");
        d.put("app.js", "x");
        d.put(".env", "A=1");
        d.put("src/lib.rs", "y");
        d.put("node_modules/react/index.js", "z");
        let l = list(&d.0, None).unwrap();
        assert_eq!(
            paths(&l),
            [(".env", None), ("app.js", None), ("src/lib.rs", None)]
        );
    }

    #[test]
    fn con_git_marca_lo_que_cambio_y_muestra_el_env() {
        let d = Dir::new("con-git");
        d.git(&["init", "-q", "-b", "main"]);
        d.git(&["config", "user.name", "pastor"]);
        d.git(&["config", "user.email", "pastor@example.com"]);
        d.put(".gitignore", ".env\nnode_modules/\nsecretos/\n");
        d.put("app.js", "uno\n");
        d.put("viejo.txt", "x\n");
        d.git(&["add", "."]);
        d.git(&["commit", "-qm", "inicio"]);
        d.put("app.js", "dos\n");
        d.put("nuevo.js", "n\n");
        d.put(".env", "A=1\n");
        d.put("node_modules/x/index.js", "z");
        // Sin ignorar, tampoco: pesan igual.
        d.put("web/dist/bundle.js", "z");
        d.put("secretos/clave.txt", "k");
        fs::remove_file(d.0.join("viejo.txt")).unwrap();
        let l = list(&d.0, None).unwrap();
        assert_eq!(
            paths(&l),
            [
                (".env", None),
                (".gitignore", None),
                ("app.js", Some(Change::Modified)),
                ("nuevo.js", Some(Change::Added)),
                ("secretos/clave.txt", None),
            ]
        );
    }

    #[test]
    fn lee_y_guarda_solo_si_nadie_lo_cambio() {
        let d = Dir::new("guarda");
        d.put("a.txt", "uno\n");
        let o = read(&d.0, "a.txt").unwrap();
        assert_eq!(o.text, "uno\n");
        let h = write(&d.0, "a.txt", "dos\n", &o.hash).unwrap();
        assert_eq!(fs::read_to_string(d.0.join("a.txt")).unwrap(), "dos\n");
        // Con el hash viejo ya no: el archivo cambió.
        let e = write(&d.0, "a.txt", "tres\n", &o.hash).unwrap_err();
        assert!(e.to_string().contains("cambió"), "{e}");
        write(&d.0, "a.txt", "tres\n", &h).unwrap();
    }

    #[test]
    fn no_sale_del_directorio() {
        let d = Dir::new("fuera");
        d.put("dentro/a.txt", "a");
        d.put("b.bin", "a\0b");
        let root = d.0.join("dentro");
        std::os::unix::fs::symlink("/etc/hostname", root.join("enlace")).unwrap();
        for bad in ["../b.bin", "/etc/hostname", "enlace", "", "."] {
            assert!(read(&root, bad).is_err(), "{bad}");
        }
        assert!(read(&d.0, "b.bin")
            .unwrap_err()
            .to_string()
            .contains("no es texto"));
        assert!(read(&root, "./a.txt").is_ok());
    }
}
