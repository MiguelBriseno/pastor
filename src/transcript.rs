//! La conversación de un agente de Claude Code, leída de su transcripción:
//! un JSONL que Claude Code escribe y que solo crece. La pantalla de tmux
//! solo tiene lo visible, y Claude Code borra el historial de la terminal al
//! redibujarse, así que para ver la conversación entera hay que ir aquí.
//!
//! El formato es interno de Claude Code: se lee con cuidado y se ignora lo
//! que no se entiende.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// `user`, `assistant` o `tool` (una herramienta que usó el agente).
    pub role: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// De una herramienta, su entrada entera (el comando, la ruta, el
    /// texto que escribió…), para buscar en ella: `text` es solo un resumen.
    #[serde(skip)]
    pub input: Option<String>,
}

/// Comienzos de lo que Claude Code inyecta como mensaje del usuario.
const INJECTED: [&str; 7] = [
    "<command-",
    "<local-command-",
    "<bash-",
    "<system-reminder>",
    "<task-notification>",
    "<user-prompt-submit-hook>",
    "<persisted-output>",
];

/// Los mensajes de una línea del JSONL: los tuyos, las respuestas del agente
/// y una línea por herramienta. Fuera el razonamiento, los resultados de las
/// herramientas, los subagentes y los registros internos.
// Sin `serve`, solo la búsqueda lee transcripciones, con `parse_line_of`.
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
pub fn parse_line(line: &str) -> Vec<Message> {
    parse_line_of(line, false)
}

/// Como `parse_line`; con `subagent`, la línea es de la transcripción de un
/// subagente, donde todas van marcadas como `isSidechain`.
pub fn parse_line_of(line: &str, subagent: bool) -> Vec<Message> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    // Subagentes, registros internos y el resumen que Claude Code escribe al
    // compactar (llega como un mensaje tuyo).
    let flag = |k: &str| v[k].as_bool() == Some(true);
    if (!subagent && flag("isSidechain"))
        || ["isMeta", "isCompactSummary", "isVisibleInTranscriptOnly"]
            .iter()
            .any(|k| flag(k))
    {
        return Vec::new();
    }
    let at = v["timestamp"].as_str().map(String::from);
    let msg = |role, text: String| Message {
        role,
        text,
        at: at.clone(),
        input: None,
    };
    let content = &v["message"]["content"];
    match v["type"].as_str() {
        Some("user") => {
            let text = match content {
                Value::String(s) => s.clone(),
                // Con imágenes o varios bloques: solo el texto. Un resultado
                // de herramienta también llega como `user`, y no es tuyo.
                Value::Array(blocks) => blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let text = text.trim();
            // Lo que Claude Code mete como si lo hubieras escrito tú: avisos
            // del sistema, salidas de comandos, notificaciones… Solo esas
            // etiquetas: un mensaje tuyo puede empezar con `<div>`.
            if text.is_empty() || INJECTED.iter().any(|t| text.starts_with(t)) {
                return Vec::new();
            }
            vec![msg("user", text.to_string())]
        }
        Some("assistant") => content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| match b["type"].as_str()? {
                "text" => {
                    let t = b["text"].as_str()?.trim();
                    (!t.is_empty()).then(|| msg("assistant", t.to_string()))
                }
                "tool_use" => Some(Message {
                    input: Some(input_text(&b["input"])),
                    ..msg("tool", tool_summary(b))
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Los textos de la entrada de una herramienta, uno por línea.
fn input_text(input: &Value) -> String {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(input, &mut out);
    out.join("\n")
}

/// `Bash · lo que hace`: el nombre y lo más claro de su entrada.
fn tool_summary(block: &Value) -> String {
    let name = block["name"].as_str().unwrap_or("herramienta");
    let input = &block["input"];
    let detail = [
        "description",
        "command",
        "file_path",
        "pattern",
        "url",
        "query",
        "prompt",
    ]
    .iter()
    .find_map(|k| input[k].as_str())
    .map(|d| d.lines().next().unwrap_or_default().trim().to_string());
    match detail {
        Some(d) if !d.is_empty() => format!("{name} · {}", crate::text::truncate(&d, 140)),
        _ => name.to_string(),
    }
}

/// La transcripción del agente `id`, si un hook de Claude Code ya la dio.
pub fn path_for(id: &str) -> Option<PathBuf> {
    let p = PathBuf::from(crate::hooks::read_status(id)?.transcript?);
    // Viene de un archivo nuestro, pero que sea lo que dice ser.
    (p.is_absolute() && p.extension().is_some_and(|e| e == "jsonl")).then_some(p)
}

/// Lee una transcripción por partes: recuerda hasta dónde llegó y solo lee
/// lo nuevo. Una de una sesión larga pasa de 10 MB.
#[derive(Default)]
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
pub struct Reader {
    path: PathBuf,
    offset: u64,
    /// El final de una línea que aún no termina de escribirse.
    rest: Vec<u8>,
    pub messages: Vec<Message>,
    /// Cuántas veces empezó de nuevo porque el archivo se reescribió.
    pub resets: u32,
}

#[cfg_attr(not(feature = "serve"), allow(dead_code))]
impl Reader {
    pub fn update(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path)
            .with_context(|| format!("no pude abrir la transcripción {}", path.display()))?;
        let len = f.metadata()?.len();
        // Otra transcripción (`/clear`, otra sesión) o una que se reescribió.
        if path != self.path || len < self.offset {
            *self = Reader {
                path: path.to_path_buf(),
                resets: self.resets + 1,
                ..Reader::default()
            };
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let read = f.take(len - self.offset).read_to_end(&mut self.rest)?;
        self.offset += read as u64;
        if let Some(end) = self.rest.iter().rposition(|&b| b == b'\n') {
            let done: Vec<u8> = self.rest.drain(..=end).collect();
            for line in String::from_utf8_lossy(&done).lines() {
                self.messages.extend(parse_line(line));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn toma_tus_mensajes_las_respuestas_y_las_herramientas() {
        let lines = [
            r#"{"type":"user","timestamp":"t1","message":{"role":"user","content":"hola"}}"#,
            r#"{"type":"user","message":{"content":"<local-command-stdout>x</local-command-stdout>"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}]}}"#,
            r#"{"type":"assistant","timestamp":"t2","message":{"content":[{"type":"text","text":"Listo."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls","description":"Lista archivos"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"a b"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagente"}]}}"#,
            r#"{"type":"api-request","foo":1}"#,
            r#"{"type":"user","isCompactSummary":true,"message":{"content":"This session is being continued"}}"#,
            r#"{"type":"user","message":{"content":"<div> no se ve bien"}}"#,
            "no es json",
        ];
        let got: Vec<_> = lines.iter().flat_map(|l| parse_line(l)).collect();
        let got: Vec<_> = got.iter().map(|m| (m.role, m.text.as_str())).collect();
        assert_eq!(
            got,
            [
                ("user", "hola"),
                ("assistant", "Listo."),
                ("tool", "Bash · Lista archivos"),
                ("user", "<div> no se ve bien"),
            ]
        );
    }

    #[test]
    fn lee_solo_lo_nuevo_y_espera_las_lineas_completas() {
        let dir = std::env::temp_dir().join(format!("pastor-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let user = |t: &str| format!(r#"{{"type":"user","message":{{"content":"{t}"}}}}"#);
        let mut f = File::create(&path).unwrap();
        writeln!(f, "{}", user("uno")).unwrap();
        // A medio escribir: no cuenta hasta que llegue el salto de línea.
        write!(f, "{}", &user("dos")[..10]).unwrap();
        f.flush().unwrap();

        let mut r = Reader::default();
        r.update(&path).unwrap();
        assert_eq!(r.messages.len(), 1);
        writeln!(f, "{}", &user("dos")[10..]).unwrap();
        f.flush().unwrap();
        r.update(&path).unwrap();
        let texts: Vec<_> = r.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["uno", "dos"]);

        // Si se reescribe más corta, empieza de nuevo.
        std::fs::write(&path, format!("{}\n", user("otra"))).unwrap();
        r.update(&path).unwrap();
        let texts: Vec<_> = r.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["otra"]);
        assert_eq!(r.resets, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
