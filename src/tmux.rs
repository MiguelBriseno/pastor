//! Capa fina sobre tmux. Usamos un socket propio (`-L pastor`) para no
//! mezclarnos con las sesiones de tmux del usuario.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::store::unique_suffix;

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
        .args(args.iter().map(|a| arg(a)))
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
    let Ok(out) = Command::new("tmux").arg("-V").output() else {
        bail!("tmux no está instalado (sudo apt install tmux)");
    };
    // `new-session -e` llegó en tmux 3.2.
    let version = String::from_utf8_lossy(&out.stdout);
    if let Some((major, minor)) = parse_version(&version) {
        if (major, minor) < (3, 2) {
            bail!(
                "pastor necesita tmux 3.2 o superior (tienes {})",
                version.trim()
            );
        }
    }
    Ok(())
}

/// Lee "tmux 3.5a" como (3, 5). `None` si no lo entiende (p. ej. "tmux
/// master"): mejor intentarlo que bloquear una versión de desarrollo.
fn parse_version(s: &str) -> Option<(u32, u32)> {
    let v = s.split_whitespace().nth(1)?;
    let v = v.strip_prefix("next-").unwrap_or(v);
    let (major, rest) = v.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// tmux toma un argumento que termina en `;` como separador de comandos y
/// se lo come; escapado con `\;` lo deja tal cual. `run` lo aplica a todos.
fn arg(s: &str) -> String {
    match s.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => s.to_string(),
    }
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
    // proceso en primer plano. `CLAUDECODE` fuera: si pastor se abrió desde
    // Claude Code, el agente no es un `claude` lanzado por otro.
    let script = format!(
        "unset CLAUDECODE\n{agent_cmd}\ntmux set-option -p -t \"$TMUX_PANE\" @pastor_exited 1 2>/dev/null\nprintf '\\n\\033[2m%s\\033[0m\\n\\n' {}\n{then}",
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
        ("status-right", status_right(false)),
        ("status-right-length", "90"),
    ] {
        run(&["set-option", "-t", &target, opt, val])?;
    }
    Ok(())
}

/// Ayuda de la barra de estado, con el prefijo real (puede que lo hayas
/// cambiado en tu ~/.tmux.conf). Dentro de otro tmux con el mismo prefijo
/// (`nested`), el de fuera se lo queda: hay que pulsarlo dos veces.
fn status_right(nested: bool) -> &'static str {
    if nested {
        " #{prefix} ←→ cambiar panel · #{prefix} #{prefix} d volver a pastor "
    } else {
        " #{prefix} ←→ cambiar panel · #{prefix} d volver a pastor "
    }
}

/// Marca un panel como `agent` o `term` para encontrarlo después.
fn set_role(pane: &str, role: &str) -> Result<()> {
    run(&["set-option", "-p", "-t", pane, "@pastor_role", role]).map(|_| ())
}

/// Recuerda en el panel el comando con el que se abrió.
pub fn set_cmd(pane: &str, cmd: &str) -> Result<()> {
    run(&["set-option", "-p", "-t", pane, "@pastor_cmd", cmd]).map(|_| ())
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
    /// Comando con el que se abrió la terminal secundaria (vacío si ninguno).
    pub cmd: String,
}

/// Formato del tamaño de un panel (`Pane::size`), p. ej. `80x24`.
const SIZE: &str = "#{pane_width}x#{pane_height}";

/// Paneles de cada sesión. Vacío si el servidor no está corriendo.
pub fn panes() -> HashMap<String, Vec<Pane>> {
    let format = format!(
        "#{{session_name}}\t#{{pane_id}}\t#{{pane_dead}}\t{SIZE}\t#{{pane_current_command}}\t#{{@pastor_exited}}\t#{{@pastor_role}}\t#{{pane_pid}}\t#{{@pastor_cmd}}"
    );
    let Ok(out) = run(&["list-panes", "-a", "-F", &format]) else {
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
            // Lo último: el comando puede llevar tabuladores.
            cmd: f.collect::<Vec<_>>().join("\t"),
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
    // Con `--`, una línea que empieza por `-` no se toma como opción.
    run(&["send-keys", "-t", pane, "-l", "--", line])?;
    run(&["send-keys", "-t", pane, "Enter"]).map(|_| ())
}

pub fn kill_pane(pane: &str) -> Result<()> {
    run(&["kill-pane", "-t", pane]).map(|_| ())
}

/// Texto visible del panel.
pub fn capture(target: &str) -> Result<String> {
    run(&["capture-pane", "-p", "-J", "-t", target])
}

/// Tamaño del panel con el mismo formato que `Pane::size`.
pub fn pane_size(target: &str) -> Result<String> {
    Ok(run(&["display-message", "-p", "-t", target, SIZE])?
        .trim()
        .to_string())
}

/// Pega `text` en el agente (bracketed paste) y pulsa Enter.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    // Único por envío: dos envíos a la vez al mismo panel (la TUI y la CLI)
    // no se pisan el buffer.
    let buf = format!("pastor-{name}-{}", unique_suffix());
    let mut child = tmux()
        .args(["load-buffer", "-b", &buf, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("no pude ejecutar tmux")?;
    // Cerrar la entrada y esperarlo aunque la escritura falle, para que no
    // quede un proceso zombi. Si tmux salió antes de leerla, su error explica
    // más que el «Broken pipe» de la escritura.
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    let status = child.wait()?;
    if !status.success() {
        bail!("tmux load-buffer falló ({status})");
    }
    written.context("no pude pasarle el texto a tmux load-buffer")?;
    run(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", name])?;
    thread::sleep(Duration::from_millis(200));
    run(&["send-keys", "-t", name, "Enter"])?;
    Ok(())
}

/// Se engancha a la sesión en la terminal actual (bloquea hasta prefijo d),
/// con el foco en `focus` si se indica.
pub fn attach(name: &str, focus: Option<&str>) -> Result<()> {
    if let Some(pane) = focus {
        run(&["select-pane", "-t", pane])?;
    }
    let nested = std::env::var_os("TMUX").is_some() && same_prefix_outside();
    let target = format!("{}:", session(name));
    let _ = run(&[
        "set-option",
        "-t",
        &target,
        "status-right",
        status_right(nested),
    ]);
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

/// Si el tmux en el que corre pastor usa el mismo prefijo que el suyo.
fn same_prefix_outside() -> bool {
    let prefix = |c: &mut Command| {
        c.args(["show-options", "-gv", "prefix"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Sin `-L`, tmux usa el servidor de `$TMUX`: el de fuera.
    let outside = prefix(&mut Command::new("tmux"));
    outside.is_none() || outside == prefix(&mut tmux())
}

/// Procesos con los que arrancaron los paneles de la sesión `name`.
pub fn pane_pids(name: &str) -> Result<Vec<u32>> {
    Ok(run(&[
        "list-panes",
        "-s",
        "-t",
        &session(name),
        "-F",
        "#{pane_pid}",
    ])?
    .lines()
    .filter_map(|l| l.trim().parse().ok())
    .collect())
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
    use super::{
        arg, capture, kill_session, new_session, panes, parse_version, run, send_text, shell_quote,
        split_right, type_line,
    };
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::thread;
    use std::time::Duration;

    /// Sin tmux se omite, salvo en CI, donde se instala. Mientras se tenga lo
    /// que devuelve, ninguna otra prueba usa el servidor: si una matara su
    /// última sesión mientras otra crea la suya, el servidor saldría a medias.
    fn have_tmux() -> Option<MutexGuard<'static, ()>> {
        static SERIAL: Mutex<()> = Mutex::new(());
        if Command::new("tmux").arg("-V").output().is_ok() {
            return Some(SERIAL.lock().unwrap_or_else(PoisonError::into_inner));
        }
        assert!(std::env::var_os("CI").is_none(), "falta tmux");
        eprintln!("sin tmux; se omite");
        None
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
        let Some(_serial) = have_tmux() else {
            return;
        };
        let s = Session(format!("nueva{}", std::process::id()));
        let cmd = ["sleep".to_string(), "30".to_string()];
        new_session(&s.0, Path::new("/"), &[], &cmd, "true").unwrap();
        assert!(panes().contains_key(&s.0));
    }

    #[test]
    fn matar_una_sesion_no_toca_otra_que_empieza_igual() {
        let Some(_serial) = have_tmux() else {
            return;
        };
        // Con el pid, una ejecución anterior interrumpida no estorba.
        let pid = std::process::id();
        let s = Session(format!("prueba{pid}-12"));
        run(&["new-session", "-d", "-s", &s.0, "sleep", "30"]).unwrap();
        assert!(kill_session(&format!("prueba{pid}-1")).is_err());
        assert!(panes().contains_key(&s.0));
    }

    /// Espera hasta 5 s a que la pantalla de `pane` cumpla `ok`.
    fn screen_until(pane: &str, ok: impl Fn(&str) -> bool) -> String {
        let mut screen = String::new();
        for _ in 0..50 {
            screen = capture(pane).unwrap_or_default();
            if ok(&screen) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        screen
    }

    #[test]
    fn el_texto_y_la_terminal_llegan_a_su_panel() {
        let Some(_serial) = have_tmux() else {
            return;
        };
        let s = Session(format!("paneles{}", std::process::id()));
        // `cat` devuelve lo que recibe: hace de agente.
        new_session(&s.0, Path::new("/"), &[], &["cat".to_string()], "true").unwrap();
        let agent = panes().remove(&s.0).unwrap().remove(0);
        assert_eq!(agent.role, "agent");

        let term = split_right(&agent.id, Path::new("/")).unwrap();
        let mut list = panes().remove(&s.0).unwrap();
        list.sort_by_key(|p| p.id != agent.id);
        let roles: Vec<_> = list
            .iter()
            .map(|p| (p.id.as_str(), p.role.as_str()))
            .collect();
        assert_eq!(
            roles,
            [(agent.id.as_str(), "agent"), (term.as_str(), "term")]
        );

        // Por su id, el texto va al agente aunque la terminal se abrió después.
        send_text(&agent.id, "hola «pastor» ñ").unwrap();
        let screen = screen_until(&agent.id, |sc| sc.matches("hola «pastor» ñ").count() >= 2);
        assert_eq!(screen.matches("hola «pastor» ñ").count(), 2, "{screen}");
        type_line(&term, "echo desde-la-terminal").unwrap();
        let screen = screen_until(&term, |sc| sc.lines().any(|l| l == "desde-la-terminal"));
        assert!(screen.lines().any(|l| l == "desde-la-terminal"), "{screen}");
        // Ni un `-` al principio ni un `;` al final se pierden por el camino.
        type_line(&term, "printf '%s\\n' -n 'otra;'").unwrap();
        let screen = screen_until(&term, |sc| sc.lines().any(|l| l == "otra;"));
        assert!(screen.lines().any(|l| l == "-n"), "{screen}");
        assert!(screen.lines().any(|l| l == "otra;"), "{screen}");
        assert!(send_text(&agent.id, "").is_err());
        assert!(!capture(&agent.id).unwrap().contains("desde-la-terminal"));
    }

    #[test]
    fn lee_la_version_de_tmux() {
        assert_eq!(parse_version("tmux 3.5a\n"), Some((3, 5)));
        assert_eq!(parse_version("tmux 3.1c"), Some((3, 1)));
        assert_eq!(parse_version("tmux next-3.6"), Some((3, 6)));
        assert_eq!(parse_version("tmux master"), None);
    }

    #[test]
    fn escapa_el_punto_y_coma_final() {
        assert_eq!(arg("npm run dev;"), "npm run dev\\;");
        assert_eq!(arg("a;b"), "a;b");
        // `run` lo aplica: la opción guarda el `;`.
        if let Some(_serial) = have_tmux() {
            let s = format!("puntoycoma{}", std::process::id());
            run(&["new-session", "-d", "-s", &s, "sleep", "30"]).unwrap();
            let pane = run(&["display", "-p", "-t", &format!("={s}:"), "#{pane_id}"]).unwrap();
            super::set_cmd(pane.trim(), "npm run dev;").unwrap();
            let got = run(&["show-options", "-pqv", "-t", pane.trim(), "@pastor_cmd"]);
            let _ = kill_session(&s);
            assert_eq!(got.unwrap().trim(), "npm run dev;");
        }
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
