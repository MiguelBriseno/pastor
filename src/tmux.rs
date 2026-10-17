//! Capa fina sobre tmux. Usamos un socket propio (`-L pastor`) para no
//! mezclarnos con las sesiones de tmux del usuario.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

// Las pruebas usan su propio servidor, para no tocar los agentes de verdad.
const SOCKET: &str = if cfg!(test) { "pastor-test" } else { "pastor" };

fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", SOCKET]);
    c
}

/// Destino de la sesión `name` y solo de ella: sin `=`, si no existe, tmux
/// acepta cualquier sesión que empiece igual (`a1` llegaría a `a12`).
fn session(name: &str) -> String {
    format!("={name}")
}

fn run(args: &[&str]) -> Result<String> {
    let out = tmux()
        .args(args)
        .output()
        .context("no pude ejecutar tmux")?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn ensure_available() -> Result<()> {
    if Command::new("tmux").arg("-V").output().is_err() {
        bail!("tmux no está instalado (sudo apt install tmux)");
    }
    Ok(())
}

/// Crea una sesión desacoplada que ejecuta `cmd` en `cwd`. Cuando `cmd`
/// termina, se ejecuta `then` (código de `sh`), que deja abierto un shell
/// para poder volver a lanzar el agente.
pub fn new_session(
    name: &str,
    cwd: &Path,
    env: &[(&str, &str)],
    cmd: &[String],
    then: &str,
) -> Result<()> {
    let cwd = cwd.to_string_lossy();
    let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let agent_cmd = cmd
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    // Al salir el agente marcamos el panel (`$TMUX` apunta a nuestro
    // servidor). Sin la marca no se puede saber si salió: mientras corre,
    // `sh` comparte grupo de procesos con él y tmux reporta "sh" como el
    // proceso en primer plano.
    let script = format!(
        "{agent_cmd}\ntmux set-option -p -t \"$TMUX_PANE\" @pastor_exited 1 2>/dev/null\nprintf '\\n\\033[2m%s\\033[0m\\n\\n' {}\n{then}",
        shell_quote("El agente terminó. Escribe claude, codex u opencode para abrirlo de nuevo."),
    );

    let mut args = vec![
        "new-session",
        "-d",
        "-s",
        name,
        "-c",
        &cwd,
        "-x",
        "200",
        "-y",
        "50",
    ];
    for e in &env {
        args.extend(["-e", e.as_str()]);
    }
    args.extend(["-P", "-F", "#{pane_id}"]);
    // Varios argumentos: tmux ejecuta `sh` directamente, sin pasar por el
    // shell del usuario (que podría ser fish).
    args.extend(["sh", "-c", &script, "pastor"]);
    let pane = run(&args)?.trim().to_string();
    set_role(&pane, "agent")?;

    // Si hasta el shell termina, dejamos el panel visible para poder leer su salida.
    // `set-option -t` espera un panel: `=a1` solo, tmux lo busca como panel
    // y no lo encuentra; con `:` es la ventana actual de esa sesión.
    let target = format!("{}:", session(name));
    run(&["set-option", "-w", "-t", &target, "remain-on-exit", "on"])?;
    run(&["set-option", "-g", "focus-events", "on"])?;
    for (opt, val) in [
        ("mouse", "on"),
        ("status-style", "bg=colour236,fg=colour250"),
        ("status-left", " pastor:#S "),
        ("status-left-length", "30"),
        (
            "status-right",
            " Ctrl-b ←→ cambiar panel · Ctrl-b d volver a pastor ",
        ),
        ("status-right-length", "60"),
    ] {
        run(&["set-option", "-t", &target, opt, val])?;
    }
    Ok(())
}

/// Marca un panel como `agent` o `term` para encontrarlo después.
fn set_role(pane: &str, role: &str) -> Result<()> {
    run(&["set-option", "-p", "-t", pane, "@pastor_role", role]).map(|_| ())
}

#[derive(Clone, Debug)]
pub struct Pane {
    pub id: String,
    pub dead: bool,
    /// Ancho y alto; si cambian, el redibujo no es actividad del agente.
    pub size: String,
    /// "agent", "term" o vacío (agentes creados antes de existir las marcas).
    pub role: String,
    /// Proceso en primer plano (p. ej. "claude" o "bash").
    pub command: String,
    /// Proceso con el que arrancó el panel; de él cuelga todo lo que corre ahí.
    pub pid: Option<u32>,
    /// El primer agente que se lanzó en el panel ya salió.
    pub exited: bool,
}

/// Paneles de cada sesión. Vacío si el servidor no está corriendo.
pub fn panes() -> HashMap<String, Vec<Pane>> {
    let format =
        "#{session_name}\t#{pane_id}\t#{pane_dead}\t#{pane_width}x#{pane_height}\t#{pane_current_command}\t#{@pastor_exited}\t#{@pastor_role}\t#{pane_pid}";
    let Ok(out) = run(&["list-panes", "-a", "-F", format]) else {
        return HashMap::new();
    };
    let mut map: HashMap<String, Vec<Pane>> = HashMap::new();
    for line in out.lines() {
        let mut f = line.split('\t');
        let (Some(session), Some(id), Some(dead), Some(size), Some(command)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        map.entry(session.to_string()).or_default().push(Pane {
            id: id.to_string(),
            dead: dead == "1",
            size: size.to_string(),
            command: command.to_string(),
            exited: f.next() == Some("1"),
            role: f.next().unwrap_or_default().to_string(),
            pid: f.next().and_then(|p| p.parse().ok()),
        });
    }
    map
}

/// Abre una terminal a la derecha de `pane` (sin quitarle el foco) y devuelve su id.
pub fn split_right(pane: &str, cwd: &Path) -> Result<String> {
    let cwd = cwd.to_string_lossy();
    let args = [
        "split-window",
        "-h",
        "-d",
        "-l",
        "40%",
        "-t",
        pane,
        "-c",
        &cwd,
        "-P",
        "-F",
        "#{pane_id}",
    ];
    let id = run(&args)?.trim().to_string();
    set_role(&id, "term")?;
    // La ventana tiene remain-on-exit para el agente; la terminal, en cambio,
    // se cierra sola al hacer `exit`.
    run(&["set-option", "-p", "-t", &id, "remain-on-exit", "off"])?;
    Ok(id)
}

/// Escribe `line` en el panel y pulsa Enter, como si lo tecleara el usuario.
pub fn type_line(pane: &str, line: &str) -> Result<()> {
    run(&["send-keys", "-t", pane, "-l", line])?;
    run(&["send-keys", "-t", pane, "Enter"]).map(|_| ())
}

pub fn kill_pane(pane: &str) -> Result<()> {
    run(&["kill-pane", "-t", pane]).map(|_| ())
}

/// Texto visible del panel.
pub fn capture(target: &str) -> Result<String> {
    run(&["capture-pane", "-p", "-J", "-t", target])
}

/// Pega `text` en el agente (bracketed paste) y pulsa Enter.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    let buf = format!("pastor-{name}");
    let mut child = tmux()
        .args(["load-buffer", "-b", &buf, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("no pude ejecutar tmux")?;
    // Cerrar la entrada y esperarlo aunque la escritura falle, para que no
    // quede un proceso zombi.
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    let status = child.wait()?;
    written?;
    if !status.success() {
        bail!("tmux load-buffer falló");
    }
    run(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", name])?;
    thread::sleep(Duration::from_millis(200));
    run(&["send-keys", "-t", name, "Enter"])?;
    Ok(())
}

/// Se engancha a la sesión en la terminal actual (bloquea hasta Ctrl-b d),
/// con el foco en `focus` si se indica.
pub fn attach(name: &str, focus: Option<&str>) -> Result<()> {
    if let Some(pane) = focus {
        run(&["select-pane", "-t", pane])?;
    }
    let status = tmux()
        .args(["attach-session", "-t", &session(name)])
        .env_remove("TMUX")
        .status()
        .context("no pude ejecutar tmux")?;
    if !status.success() {
        bail!("no pude entrar a la sesión {name}");
    }
    Ok(())
}

pub fn kill_session(name: &str) -> Result<()> {
    run(&["kill-session", "-t", &session(name)]).map(|_| ())
}

pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::{kill_session, new_session, panes, run, shell_quote};
    use std::path::Path;
    use std::process::Command;

    /// Sin tmux se omite, salvo en CI, donde se instala.
    fn have_tmux() -> bool {
        if Command::new("tmux").arg("-V").output().is_ok() {
            return true;
        }
        assert!(std::env::var_os("CI").is_none(), "falta tmux");
        eprintln!("sin tmux; se omite");
        false
    }

    /// Se mata aunque la prueba falle.
    struct Session(String);
    impl Drop for Session {
        fn drop(&mut self) {
            let _ = kill_session(&self.0);
        }
    }

    #[test]
    fn crea_y_configura_una_sesion() {
        if !have_tmux() {
            return;
        }
        let s = Session(format!("nueva{}", std::process::id()));
        let cmd = ["sleep".to_string(), "30".to_string()];
        new_session(&s.0, Path::new("/"), &[], &cmd, "true").unwrap();
        assert!(panes().contains_key(&s.0));
    }

    #[test]
    fn matar_una_sesion_no_toca_otra_que_empieza_igual() {
        if !have_tmux() {
            return;
        }
        // Con el pid, una ejecución anterior interrumpida no estorba.
        let pid = std::process::id();
        let s = Session(format!("prueba{pid}-12"));
        run(&["new-session", "-d", "-s", &s.0, "sleep", "30"]).unwrap();
        assert!(kill_session(&format!("prueba{pid}-1")).is_err());
        assert!(panes().contains_key(&s.0));
    }

    #[test]
    fn deja_igual_lo_seguro() {
        assert_eq!(shell_quote("/usr/bin/claude"), "/usr/bin/claude");
        assert_eq!(shell_quote("--settings=a.json"), "--settings=a.json");
    }

    #[test]
    fn entrecomilla_lo_demas() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("mi carpeta"), "'mi carpeta'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    /// Lo que entra sale idéntico al pasar por `sh`, sin ejecutar nada.
    #[test]
    fn sh_recupera_el_texto_original() {
        for s in [
            "",
            "a b",
            "it's",
            "'",
            "''",
            "$HOME `id` $(id) ; | & > < * ? ~ \\ \"",
            "línea\nnueva\ttab",
            "ñandú 🦀",
        ] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
    }
}
