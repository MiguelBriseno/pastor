//! Avisos al celular con [ntfy](https://ntfy.sh): los mismos que salen en el
//! escritorio, publicados en un tema. Se activa con `PASTOR_NTFY_URL`
//! (p. ej. `https://ntfy.sh/mi-tema-secreto`) y, si el tema lo pide,
//! `PASTOR_NTFY_TOKEN`.

use std::io::Write;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};

struct Config {
    url: String,
    token: Option<String>,
}

fn config() -> Option<Config> {
    let var = |k| {
        std::env::var(k)
            .ok()
            .filter(|v: &String| !v.trim().is_empty())
    };
    Some(Config {
        url: var("PASTOR_NTFY_URL")?,
        token: var("PASTOR_NTFY_TOKEN"),
    })
}

/// Un botón del aviso: abre una página o llama a `pastor serve`.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    View { label: String, url: String },
    Post { label: String, url: String },
}

/// La URL con la que el celular llega a `pastor serve` (p. ej.
/// `http://100.64.1.2:7433`), sin la `/` del final. Con ella los avisos
/// llevan botones.
pub fn serve_url() -> Option<String> {
    let url = std::env::var("PASTOR_SERVE_URL").ok()?;
    let url = url.trim().trim_end_matches('/');
    (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_string())
}

/// Publica `msg` si ntfy está configurado, con `actions` como botones.
/// Devuelve el `curl` en marcha para que quien llama lo espere (o lo mate si
/// tarda).
pub fn publish(msg: &str, actions: &[Action]) -> Option<Child> {
    let config = config()?;
    let mut child = Command::new("curl")
        // La configuración por la entrada: así el token no se ve en `ps`.
        .args(["-fsS", "-m", "10", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // Para `check`; con `-sS` es solo una línea, no llena la tubería.
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let input = curl_config(&config, msg, actions);
    let written = child
        .stdin
        .take()
        .map(|mut s| s.write_all(input.as_bytes()));
    if !matches!(written, Some(Ok(()))) {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some(child)
}

/// Anota en `hooks.log` un aviso que no salió: si no, un token o una URL
/// mal puestos dan cero avisos sin ninguna pista. `status` es cómo terminó
/// curl, o `None` si hubo que matarlo; `stderr`, lo que dijo.
pub fn check(stderr: Option<ChildStderr>, status: Option<ExitStatus>) {
    if let Some(why) = failure(stderr, status) {
        crate::hooks::log_error("ntfy", &anyhow::anyhow!("no pude publicar el aviso: {why}"));
    }
}

fn failure(stderr: Option<ChildStderr>, status: Option<ExitStatus>) -> Option<String> {
    use std::io::Read;
    match status {
        Some(s) if s.success() => None,
        None => Some("no respondió a tiempo".into()),
        Some(_) => {
            let mut err = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut err);
            }
            Some(match err.trim() {
                "" => "curl falló".into(),
                e => e.into(),
            })
        }
    }
}

fn curl_config(c: &Config, msg: &str, actions: &[Action]) -> String {
    let mut out = format!("url = {}\n", quote(&c.url));
    out.push_str("header = \"Title: pastor\"\n");
    if !actions.is_empty() {
        out.push_str(&format!(
            "header = {}\n",
            quote(&format!("Actions: {}", actions_header(actions)))
        ));
    }
    if let Some(t) = &c.token {
        out.push_str(&format!(
            "header = {}\n",
            quote(&format!("Authorization: Bearer {t}"))
        ));
    }
    out.push_str(&format!("data-binary = {}\n", quote(msg)));
    out
}

/// Los botones en el formato de la cabecera `Actions` de ntfy (como mucho
/// tres). Las etiquetas y URLs son nuestras: sin `,` ni `;`.
fn actions_header(actions: &[Action]) -> String {
    actions
        .iter()
        .take(3)
        .map(|a| match a {
            Action::View { label, url } => format!("view, {label}, {url}, clear=true"),
            Action::Post { label, url } => {
                format!("http, {label}, {url}, method=POST, clear=true")
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Una cadena entre comillas del archivo de configuración de curl.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dice_por_que_fallo_curl() {
        let run = |args: &[&str]| {
            let mut c = Command::new("curl")
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let err = c.stderr.take();
            let status = c.wait().unwrap();
            failure(err, Some(status))
        };
        let why = run(&["-fsS", "-m", "5", "http://127.0.0.1:1/"]).unwrap();
        assert!(why.starts_with("curl: (7)"), "{why}");
        assert_eq!(run(&["-fsS", "--version"]), None);
        assert_eq!(
            failure(None, None).as_deref(),
            Some("no respondió a tiempo")
        );
    }

    #[test]
    fn cita_para_curl() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(quote("uno\ndos"), r#""uno\ndos""#);
    }

    #[test]
    fn configuracion_de_curl() {
        let c = Config {
            url: "https://ntfy.sh/tema".into(),
            token: Some("tk_1".into()),
        };
        assert_eq!(
            curl_config(&c, "web terminó", &[]),
            "url = \"https://ntfy.sh/tema\"\n\
             header = \"Title: pastor\"\n\
             header = \"Authorization: Bearer tk_1\"\n\
             data-binary = \"web terminó\"\n"
        );
        let c = Config { token: None, ..c };
        assert!(!curl_config(&c, "x", &[]).contains("Authorization"));
        let actions = [
            Action::View {
                label: "Abrir".into(),
                url: "http://h:7433/#/agent/a1".into(),
            },
            Action::Post {
                label: "Aprobar".into(),
                url: "http://h:7433/act/abc".into(),
            },
        ];
        assert!(curl_config(&c, "x", &actions).contains(
            "header = \"Actions: view, Abrir, http://h:7433/#/agent/a1, clear=true; \
             http, Aprobar, http://h:7433/act/abc, method=POST, clear=true\"\n"
        ));
    }
}
