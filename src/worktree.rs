//! Un worktree de git por agente: cada uno trabaja en su propia rama y en su
//! propia copia del repositorio, así varios agentes pueden cambiar el mismo
//! repositorio a la vez sin pisarse. Los worktrees viven en
//! `~/.local/share/pastor/worktrees/` y sus ramas se llaman `pastor/<nombre>`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::store::{data_dir, Worktree};

/// `git -C dir …`, sin las variables que un hook de git dejaría puestas y
/// harían que el comando actúe sobre otro repositorio.
fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_PREFIX"] {
        c.env_remove(var);
    }
    c
}

/// Corre git y devuelve su salida sin el salto final. Si falla, el error es
/// lo que dijo git.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = command(dir)
        .args(args)
        .output()
        .context("no pude ejecutar git; ¿está instalado?")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// La raíz del repositorio de git que contiene `dir`, si está en uno.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}

/// Crea un worktree con una rama nueva que sale del commit actual del
/// repositorio de `cwd`. `name` da el nombre de la rama y de la carpeta.
/// Devuelve el worktree y dónde debe arrancar el agente: la misma
/// subcarpeta de `cwd` dentro de la copia.
pub fn create(cwd: &Path, name: &str) -> Result<(Worktree, PathBuf)> {
    create_in(&data_dir().join("worktrees"), cwd, name)
}

fn create_in(dir: &Path, cwd: &Path, name: &str) -> Result<(Worktree, PathBuf)> {
    let not_repo = || anyhow!("{} no está en un repositorio de git", cwd.display());
    // Las dos rutas canónicas: git resuelve los enlaces simbólicos y, si no,
    // `strip_prefix` no reconocería la subcarpeta.
    let cwd = cwd.canonicalize().map_err(|_| not_repo())?;
    let top = toplevel(&cwd)
        .and_then(|t| t.canonicalize().ok())
        .ok_or_else(not_repo)?;
    let base = git(&top, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| anyhow!("el repositorio {} no tiene commits todavía", top.display()))?;
    let repo_name = top
        .file_name()
        .map_or("repo".into(), |n| n.to_string_lossy().into_owned());
    fs::create_dir_all(dir)?;
    let slug = slug(name);
    let (branch, path) = (1..)
        .map(|i| {
            let name = if i == 1 {
                slug.clone()
            } else {
                format!("{slug}-{i}")
            };
            (
                format!("pastor/{name}"),
                dir.join(format!("{repo_name}-{name}")),
            )
        })
        .find(|(b, p)| !p.exists() && !branch_exists(&top, b))
        .expect("siempre hay un nombre libre");
    git(
        &top,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            &base,
        ],
    )?;
    let start = cwd
        .strip_prefix(&top)
        .ok()
        // `join("")` dejaría una `/` al final.
        .filter(|rel| !rel.as_os_str().is_empty())
        .map(|rel| path.join(rel))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| path.clone());
    let wt = Worktree {
        repo: top,
        path,
        branch,
        base,
    };
    Ok((wt, start))
}

/// Si `dir` está dentro de un repositorio de git con al menos un commit.
pub fn is_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// Un nombre que sirve para una rama y una carpeta: minúsculas, letras y
/// cifras separadas por guiones, sin acentos y no muy largo.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut out: String = out.chars().take(40).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "agente".into()
    } else {
        out
    }
}

/// Si en el worktree hay algo que se perdería al borrarlo: archivos sin
/// guardar en un commit o commits nuevos en su rama.
pub fn has_changes(wt: &Worktree) -> Result<bool> {
    if wt.path.is_dir() && !git(&wt.path, &["status", "--porcelain"])?.is_empty() {
        return Ok(true);
    }
    Ok(commits(wt)? > 0)
}

/// Cuántos commits tiene la rama desde que salió.
fn commits(wt: &Worktree) -> Result<usize> {
    if !branch_exists(&wt.repo, &wt.branch) {
        return Ok(0);
    }
    let range = format!("{}..refs/heads/{}", wt.base, wt.branch);
    Ok(git(&wt.repo, &["rev-list", "--count", &range])?
        .parse()
        .unwrap_or(0))
}

/// Borra el worktree y su rama, aunque tengan cambios: quien llama ya
/// comprobó que no los hay o que están integrados.
pub fn remove(wt: &Worktree) -> Result<()> {
    if wt.path.exists() {
        git(
            &wt.repo,
            &["worktree", "remove", "--force", &wt.path.to_string_lossy()],
        )?;
    } else {
        // Alguien borró la carpeta a mano: que git lo olvide.
        let _ = git(&wt.repo, &["worktree", "prune"]);
    }
    if branch_exists(&wt.repo, &wt.branch) {
        git(&wt.repo, &["branch", "-D", &wt.branch])?;
    }
    Ok(())
}

/// Borra el worktree solo si no tiene nada que se pueda perder.
pub fn remove_if_unchanged(wt: &Worktree) {
    if matches!(has_changes(wt), Ok(false)) {
        if let Err(e) = remove(wt) {
            crate::hooks::log_error("worktree", &e);
        }
    }
}

/// Prepara el `git diff` de todo lo que hizo el agente: commits, cambios sin
/// guardar y archivos nuevos, y se lo pasa a `run` (que lo corre en la
/// terminal, con el paginador y los colores de git, o lee su salida).
pub fn diff<T>(wt: &Worktree, stat: bool, run: impl FnOnce(Command) -> Result<T>) -> Result<T> {
    if !wt.path.is_dir() {
        bail!("el worktree {} ya no existe", wt.path.display());
    }
    // Los archivos nuevos solo salen si están en el índice. Se agregan a una
    // copia del índice, no al del worktree: el agente puede estar usándolo.
    let index = std::env::temp_dir().join(format!(
        "pastor-diff-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let result = (|| {
        let real = wt
            .path
            .join(git(&wt.path, &["rev-parse", "--git-path", "index"])?);
        // Copiar el índice ahorra volver a leer los archivos que no cambiaron.
        if real.exists() {
            fs::copy(&real, &index)?;
        }
        let out = command(&wt.path)
            .env("GIT_INDEX_FILE", &index)
            .args(["add", "--all"])
            .output()?;
        if !out.status.success() {
            bail!("git add: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let mut c = command(&wt.path);
        c.env("GIT_INDEX_FILE", &index).args(["diff", "--cached"]);
        if stat {
            c.arg("--stat");
        }
        c.arg(diff_base(wt));
        run(c)
    })();
    let _ = fs::remove_file(&index);
    result
}

/// Desde dónde se compara: el commit del que salió la rama o, si el agente
/// integró después la rama del repositorio en la suya, ese punto. Así no se
/// ven como del agente los cambios que llegaron de tu rama.
fn diff_base(wt: &Worktree) -> String {
    git(&wt.repo, &["rev-parse", "HEAD"])
        .and_then(|head| git(&wt.path, &["merge-base", "HEAD", &head]))
        .unwrap_or_else(|_| wt.base.clone())
}

/// Carpetas que pesan mucho y no se leen a mano: el editor no entra en ellas
/// aunque no estén en el `.gitignore`.
pub const HEAVY_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    "coverage",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
];

/// Las rutas de una salida de git separada por NUL (`-z`).
fn nul_split(out: &str) -> impl Iterator<Item = &str> {
    out.split('\0').filter(|s| !s.is_empty())
}

/// Si alguna carpeta de la ruta `rel` es pesada.
fn in_heavy_dir(rel: &str) -> bool {
    rel.rsplit_once('/')
        .is_some_and(|(dirs, _)| dirs.split('/').any(|d| HEAVY_DIRS.contains(&d)))
}

/// Los archivos de `dir` (que está en un repositorio), relativos a él: los del
/// repositorio y los nuevos, más los ignorados, sin los que estén en una
/// carpeta pesada aunque no esté ignorada. Así sale el `.env`, pero no
/// `node_modules`. Los del repositorio sí salen siempre: alguien los subió.
pub fn files(dir: &Path, max: usize) -> Result<Vec<String>> {
    let cached = git(dir, &["ls-files", "-z", "--cached"])?;
    let mut files: Vec<String> = nul_split(&cached).map(String::from).collect();
    let others = git(dir, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    files.extend(
        nul_split(&others)
            .filter(|p| !in_heavy_dir(p))
            .map(String::from),
    );
    let ignored = git(
        dir,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ],
    )?;
    for path in nul_split(&ignored) {
        if files.len() >= max {
            break;
        }
        match path.strip_suffix('/') {
            Some(sub) => {
                let name = sub.rsplit('/').next().unwrap_or(sub);
                if !HEAVY_DIRS.contains(&name) && !in_heavy_dir(path) {
                    walk(dir, Path::new(sub), max, &mut files);
                }
            }
            None if !in_heavy_dir(path) => files.push(path.to_string()),
            None => {}
        }
    }
    // Los borrados siguen en el índice, y un submódulo es una carpeta.
    files.retain(|f| dir.join(f).is_file());
    Ok(files)
}

/// Agrega a `out` los archivos de `dir/sub` (relativos a `dir`), sin entrar en
/// carpetas pesadas ni seguir enlaces a carpetas, hasta tener `max`.
pub fn walk(dir: &Path, sub: &Path, max: usize, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir.join(sub)) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= max {
            return;
        }
        let name = e.file_name();
        let rel = sub.join(&name);
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() {
            if !HEAVY_DIRS.contains(&name.to_string_lossy().as_ref()) {
                walk(dir, &rel, max, out);
            }
        } else if dir.join(&rel).is_file() {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

/// Lo que cambió en `dir`, relativo a él: con `wt`, todo lo del agente desde
/// que salió su rama (commits incluidos); sin él, lo que no está en un
/// commit. `true` si el archivo es nuevo. Sin los borrados: ya no se abren.
pub fn changes(dir: &Path, wt: Option<&Worktree>) -> Result<Vec<(String, bool)>> {
    let base = wt.map_or_else(|| "HEAD".to_string(), diff_base);
    let mut out = Vec::new();
    // Sin commits todavía no hay contra qué comparar: solo los nuevos.
    if let Ok(diff) = git(
        dir,
        &[
            "diff",
            "-z",
            "--name-status",
            "--no-renames",
            "--relative",
            &base,
        ],
    ) {
        let mut parts = nul_split(&diff);
        while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
            match status {
                "D" => {}
                "A" => out.push((path.to_string(), true)),
                _ => out.push((path.to_string(), false)),
            }
        }
    }
    let new = git(dir, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    out.extend(nul_split(&new).map(|p| (p.to_string(), true)));
    Ok(out)
}

/// Guarda en un commit lo que el agente dejó sin guardar e integra la rama
/// en la rama actual del repositorio. Devuelve en qué rama se integró, o
/// `None` si no había nada que integrar.
pub fn merge(wt: &Worktree, msg: &str) -> Result<Option<String>> {
    if wt.path.is_dir() && !git(&wt.path, &["status", "--porcelain"])?.is_empty() {
        git(&wt.path, &["add", "--all"])?;
        git(&wt.path, &["commit", "--quiet", "-m", msg])?;
    }
    if commits(wt)? == 0 {
        return Ok(None);
    }
    let into = git(&wt.repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|_| {
        anyhow!(
            "{} no está en una rama (HEAD separado); cámbiate a una para integrar {}",
            wt.repo.display(),
            wt.branch
        )
    })?;
    if let Err(e) = git(&wt.repo, &["merge", "--no-edit", &wt.branch]) {
        // Con conflictos git deja el merge a medias en tu repositorio: se
        // deshace para dejarlo como estaba.
        if git(
            &wt.repo,
            &["rev-parse", "--quiet", "--verify", "MERGE_HEAD"],
        )
        .is_ok()
        {
            let _ = git(&wt.repo, &["merge", "--abort"]);
            bail!(
                "{} tiene conflictos con {into}; pídele al agente que integre {into} en su rama y vuelve a aprobar",
                wt.branch
            );
        }
        return Err(e);
    }
    Ok(Some(into))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nombres_para_ramas() {
        assert_eq!(slug("Migrar login a OAuth"), "migrar-login-a-oauth");
        assert_eq!(slug("t3-¿Añadir canción?"), "t3-anadir-cancion");
        assert_eq!(slug("  --  "), "agente");
        assert_eq!(slug(&"x".repeat(60)).len(), 40);
        assert_eq!(
            slug("una frase bastante larga para una rama de git"),
            "una-frase-bastante-larga-para-una-rama-d"
        );
    }

    /// Un repositorio con un commit y la carpeta de los worktrees, que se
    /// borran al terminar.
    struct Repo(PathBuf);

    impl Repo {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("pastor-wt-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let repo = base.join("web");
            fs::create_dir_all(repo.join("src")).unwrap();
            for args in [
                &["init", "-q", "-b", "main"][..],
                &["config", "user.name", "pastor"],
                &["config", "user.email", "pastor@example.com"],
            ] {
                git(&repo, args).unwrap();
            }
            fs::write(repo.join("src/a.txt"), "uno\n").unwrap();
            git(&repo, &["add", "."]).unwrap();
            git(&repo, &["commit", "-qm", "inicio"]).unwrap();
            Repo(base)
        }

        fn repo(&self) -> PathBuf {
            self.0.join("web").canonicalize().unwrap()
        }

        fn create(&self, cwd: &Path, name: &str) -> (Worktree, PathBuf) {
            create_in(&self.0.join("worktrees"), cwd, name).unwrap()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn crea_integra_y_borra() {
        let r = Repo::new("integra");
        assert_eq!(toplevel(&r.repo().join("src")), Some(r.repo()));
        let (wt, start) = r.create(&r.repo().join("src"), "t3 Migrar login");
        assert_eq!(wt.branch, "pastor/t3-migrar-login");
        assert_eq!(wt.repo, r.repo());
        assert!(wt.path.ends_with("web-t3-migrar-login"));
        // Arranca en la misma subcarpeta, dentro de la copia.
        assert_eq!(start, wt.path.join("src"));
        // El mismo nombre otra vez no choca.
        let (otro, start) = r.create(&r.repo(), "t3 Migrar login");
        assert_eq!(start, otro.path);
        assert_eq!(otro.branch, "pastor/t3-migrar-login-2");
        assert!(!has_changes(&otro).unwrap());
        remove(&otro).unwrap();
        assert!(!otro.path.exists() && !branch_exists(&r.repo(), &otro.branch));

        // Un archivo nuevo sin commit cuenta como cambio y se integra.
        assert!(!has_changes(&wt).unwrap());
        fs::write(wt.path.join("src/b.txt"), "dos\n").unwrap();
        assert!(has_changes(&wt).unwrap());
        let stat = |wt: &Worktree| {
            let out = diff(wt, true, |mut c| Ok(c.output()?)).unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert!(stat(&wt).contains("src/b.txt"));
        // El diff no toca el índice del agente.
        assert_eq!(
            git(&wt.path, &["status", "--porcelain"]).unwrap(),
            "?? src/b.txt"
        );
        // Lo que llegó de main después de integrarla no cuenta como del agente.
        fs::write(r.repo().join("c.txt"), "de main\n").unwrap();
        git(&r.repo(), &["add", "c.txt"]).unwrap();
        git(&r.repo(), &["commit", "-qm", "en main"]).unwrap();
        git(&wt.path, &["merge", "-q", "main"]).unwrap();
        assert!(!stat(&wt).contains("c.txt"), "{}", stat(&wt));
        assert_eq!(
            merge(&wt, "[t3] Migrar login").unwrap().as_deref(),
            Some("main")
        );
        assert_eq!(
            fs::read_to_string(r.repo().join("src/b.txt")).unwrap(),
            "dos\n"
        );
        remove(&wt).unwrap();
        assert!(!wt.path.exists() && !branch_exists(&r.repo(), &wt.branch));
    }

    #[test]
    fn sin_cambios_no_integra_nada() {
        let r = Repo::new("vacio");
        let (wt, _) = r.create(&r.repo(), "nada");
        assert_eq!(merge(&wt, "x").unwrap(), None);
        remove_if_unchanged(&wt);
        assert!(!wt.path.exists());
    }

    #[test]
    fn con_conflictos_deja_el_repositorio_como_estaba() {
        let r = Repo::new("conflicto");
        let (wt, _) = r.create(&r.repo(), "choque");
        fs::write(wt.path.join("src/a.txt"), "del agente\n").unwrap();
        fs::write(r.repo().join("src/a.txt"), "tuyo\n").unwrap();
        git(&r.repo(), &["commit", "-qam", "mío"]).unwrap();
        let err = merge(&wt, "x").unwrap_err().to_string();
        assert!(err.contains("conflictos con main"), "{err}");
        assert!(git(&r.repo(), &["status", "--porcelain"])
            .unwrap()
            .is_empty());
        // Lo del agente quedó en un commit de su rama: no se pierde.
        assert!(has_changes(&wt).unwrap());
    }

    #[test]
    fn fuera_de_un_repositorio() {
        let dir = std::env::temp_dir().join("pastor-wt-nada");
        assert_eq!(toplevel(Path::new("/")), None);
        let err = create_in(&dir, Path::new("/"), "x").unwrap_err();
        assert!(
            err.to_string().contains("no está en un repositorio"),
            "{err}"
        );
    }
}
