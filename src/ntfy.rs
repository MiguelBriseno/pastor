//! Avisos al celular con [ntfy](https://ntfy.sh): los mismos que salen en el
//! escritorio, publicados en un tema. Se activa con `PASTOR_NTFY_URL`
//! (p. ej. `https://ntfy.sh/mi-tema-secreto`) y, si el tema lo pide,
//! `PASTOR_NTFY_TOKEN`.

use std::io::Write;
use std::process::{Child, Command, Stdio};

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

/// Publica `msg` si ntfy está configurado. Devuelve el `curl` en marcha para
/// que quien llama lo espere (o lo mate si tarda).
pub fn publish(msg: &str) -> Option<Child> {
    let config = config()?;
    let mut child = Command::new("curl")
        // La configuración por la entrada: así el token no se ve en `ps`.
        .args(["-fsS", "-m", "10", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let input = curl_config(&config, msg);
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

fn curl_config(c: &Config, msg: &str) -> String {
    let mut out = format!("url = {}\n", quote(&c.url));
    out.push_str("header = \"Title: pastor\"\n");
    if let Some(t) = &c.token {
        out.push_str(&format!(
            "header = {}\n",
            quote(&format!("Authorization: Bearer {t}"))
        ));
    }
    out.push_str(&format!("data-binary = {}\n", quote(msg)));
    out
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
            curl_config(&c, "web terminó"),
            "url = \"https://ntfy.sh/tema\"\n\
             header = \"Title: pastor\"\n\
             header = \"Authorization: Bearer tk_1\"\n\
             data-binary = \"web terminó\"\n"
        );
        let c = Config { token: None, ..c };
        assert!(!curl_config(&c, "x").contains("Authorization"));
    }
}
