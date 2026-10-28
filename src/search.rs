//! Buscar en las conversaciones de los agentes de Claude Code: las de los
//! que siguen vivos y las de los que ya terminaron, que quedan en el
//! historial con su transcripción (evento `session`). Codex y opencode no
//! dejan una transcripción que pastor sepa leer.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use serde::Serialize;

use crate::agent::Kind;
use crate::history::{self, Entry, Event};
use crate::store::State;
use crate::transcript;
use crate::usage;

/// Cuántos resultados se dan si no se pide otra cosa.
pub const MAX: usize = 50;

/// Una conversación donde buscar.
#[derive(Clone, Debug)]
struct Source {
    agent: String,
    name: String,
    cwd: PathBuf,
    transcript: PathBuf,
}

/// Un mensaje que coincide.
#[derive(Serialize, Clone, Debug)]
pub struct Hit {
    pub agent: String,
    pub name: String,
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// `user`, `assistant`, `tool` o `parent` (el agente principal, en la
    /// conversación de un subagente).
    pub role: &'static str,
    /// El trozo del mensaje alrededor de lo que se buscó, en una línea.
    pub snippet: String,
    /// El mensaje entero.
    pub text: String,
    /// Si el agente sigue en pastor.
    pub alive: bool,
}

impl Hit {
    /// Quién habla, en español.
    pub fn who(&self) -> &'static str {
        match self.role {
            "user" => "tú",
            "parent" => "agente principal",
            "tool" => "herramienta",
            _ => "agente",
        }
    }
}

pub struct Query {
    /// Las palabras, en minúsculas: un mensaje coincide si las tiene todas.
    words: Vec<String>,
    since: Option<DateTime<Utc>>,
    max: usize,
}

impl Query {
    pub fn new(text: &str, since: Option<DateTime<Utc>>, max: usize) -> Result<Query> {
        let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            bail!("escribe qué buscar");
        }
        Ok(Query { words, since, max })
    }

    fn matches(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.words.iter().all(|w| lower.contains(w.as_str()))
    }

    /// Si la línea cruda del JSONL puede coincidir. Más barato que leer el
    /// JSON de cada línea. Una palabra con `"` o `\` va escapada en el JSON:
    /// con esa no se puede descartar.
    fn may_match(&self, raw: &str) -> bool {
        let lower = raw.to_lowercase();
        self.words
            .iter()
            .all(|w| w.contains(['"', '\\']) || lower.contains(w.as_str()))
    }
}

/// Desde cuándo buscar: `30m`, `12h`, `7d`, `2w` (semanas) o una fecha
/// `AAAA-MM-DD`. Sin segundos: `s` sería ambiguo (¿segundos o semanas?).
pub fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(day) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(t) = day
            .and_hms_opt(0, 0, 0)
            .and_then(|t| t.and_local_timezone(Local).earliest())
        {
            return Ok(t.with_timezone(&Utc));
        }
    }
    let unit = s.chars().last().unwrap_or(' ');
    let n: u32 = s[..s.len() - unit.len_utf8().min(s.len())]
        .parse()
        .map_err(|_| anyhow::anyhow!("no entiendo «{s}»: usa 30m, 12h, 7d, 2w o AAAA-MM-DD"))?;
    let span = match unit {
        'm' => Duration::minutes(n.into()),
        'h' => Duration::hours(n.into()),
        'd' => Duration::days(n.into()),
        'w' => Duration::weeks(n.into()),
        _ => bail!("no entiendo «{s}»: usa 30m, 12h, 7d, 2w o AAAA-MM-DD"),
    };
    // Con un número enorme se saldría de las fechas posibles.
    Utc::now()
        .checked_sub_signed(span)
        .ok_or_else(|| anyhow::anyhow!("«{s}» es demasiado tiempo"))
}

/// Las conversaciones conocidas: las del historial y las de los agentes
/// vivos (también los de antes de que se anotaran en el historial). Si una
/// transcripción sale dos veces, manda lo más reciente.
fn sources(state: &State) -> Vec<Source> {
    let mut by_path: HashMap<PathBuf, Source> = HashMap::new();
    if let Ok(f) = File::open(history::path()) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if !line.contains(r#""event":"session""#) {
                continue;
            }
            if let Ok(Entry {
                event:
                    Event::Session {
                        agent,
                        name,
                        cwd,
                        transcript,
                    },
                ..
            }) = serde_json::from_str(&line)
            {
                by_path.insert(
                    transcript.clone(),
                    Source {
                        agent,
                        name,
                        cwd,
                        transcript,
                    },
                );
            }
        }
    }
    for a in state.agents.iter().filter(|a| a.kind == Kind::Claude) {
        if let Some(p) = transcript::path_for(&a.id) {
            by_path.insert(
                p.clone(),
                Source {
                    agent: a.id.clone(),
                    name: a.name.clone(),
                    cwd: a.cwd.clone(),
                    transcript: p,
                },
            );
        }
    }
    by_path.into_values().collect()
}

/// Busca en todas las conversaciones. Lo más reciente primero.
pub fn run(q: &Query) -> Result<Vec<Hit>> {
    let state = State::load()?;
    let sources = sources(&state);
    let mut hits: Vec<Hit> = Vec::new();
    for s in &sources {
        let alive = state.agents.iter().any(|a| a.id == s.agent);
        let files = std::iter::once((s.transcript.clone(), false)).chain(
            usage::subagent_files(&s.transcript)
                .into_iter()
                .map(|p| (p, true)),
        );
        for (path, subagent) in files {
            if let Err(e) = search_file(&path, subagent, q, s, alive, &mut hits) {
                // Una transcripción borrada no es un error de la búsqueda.
                if path.exists() {
                    crate::hooks::log_error("search", &e);
                }
            }
            // Cada resultado lleva el mensaje entero: con una palabra común
            // serían miles. Solo hacen falta los `max` más recientes.
            if hits.len() > q.max.saturating_mul(2).max(64) {
                keep_newest(&mut hits, q.max);
            }
        }
    }
    keep_newest(&mut hits, q.max);
    Ok(hits)
}

/// Deja los `max` más recientes, en orden. Sin fecha (no debería pasar) al
/// final.
fn keep_newest(hits: &mut Vec<Hit>, max: usize) {
    hits.sort_by_key(|h| std::cmp::Reverse(h.at));
    hits.truncate(max);
}

fn search_file(
    path: &Path,
    subagent: bool,
    q: &Query,
    s: &Source,
    alive: bool,
    hits: &mut Vec<Hit>,
) -> Result<()> {
    // Si no se tocó desde antes de `since`, no tiene nada de ese tiempo.
    if let Some(since) = q.since {
        let modified = fs::metadata(path)?.modified()?;
        if DateTime::<Utc>::from(modified) < since {
            return Ok(());
        }
    }
    let f = BufReader::new(File::open(path)?);
    for line in f.lines() {
        let line = line?;
        if !q.may_match(&line) {
            continue;
        }
        for m in transcript::parse_line_of(&line, subagent) {
            let at =
                m.at.as_deref()
                    .and_then(|t| t.parse::<DateTime<Utc>>().ok());
            if q.since.is_some_and(|since| at.is_none_or(|at| at < since)) {
                continue;
            }
            // De una herramienta, el resumen y su entrada entera: el
            // comando de un Bash con descripción no sale en el resumen.
            let text = match &m.input {
                Some(input) if !q.matches(&m.text) => format!("{}\n{input}", m.text),
                _ => m.text,
            };
            if !q.matches(&text) {
                continue;
            }
            // En la de un subagente, lo que llega como del usuario lo
            // escribió el agente principal al lanzarlo.
            let role = match m.role {
                "user" if subagent => "parent",
                r => r,
            };
            hits.push(Hit {
                agent: s.agent.clone(),
                name: s.name.clone(),
                cwd: s.cwd.clone(),
                at,
                role,
                snippet: snippet(&text, &q.words[0], 160),
                text,
                alive,
            });
        }
    }
    Ok(())
}

/// Un trozo de `text` en una línea, de unos `width` caracteres, con `word`
/// cerca del principio.
fn snippet(text: &str, word: &str, width: usize) -> String {
    let flat: Vec<char> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    let lower: Vec<char> = flat.iter().flat_map(|c| c.to_lowercase()).collect();
    let word: Vec<char> = word.chars().collect();
    // Si al pasar a minúsculas cambió el largo, las posiciones no valen.
    let at = (lower.len() == flat.len())
        .then(|| {
            lower
                .windows(word.len().max(1))
                .position(|w| w == word.as_slice())
        })
        .flatten()
        .unwrap_or(0);
    let start = at.saturating_sub(width / 4);
    let end = (start + width).min(flat.len());
    let mut out: String = flat[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < flat.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todas_las_palabras_sin_importar_mayusculas() {
        let q = Query::new("Auth  RS", None, MAX).unwrap();
        assert!(q.matches("Edit · src/auth.rs"));
        assert!(!q.matches("Edit · src/auth.ts"));
        assert!(q.may_match(r#"{"input":{"file_path":"src/AUTH.rs"}}"#));
        assert!(Query::new("  ", None, MAX).is_err());
        // Con comillas no se descarta por la línea cruda.
        let q = Query::new(r#"di "hola""#, None, MAX).unwrap();
        assert!(q.may_match(r#"{"text":"di \"hola\""}"#));
    }

    #[test]
    fn busca_en_la_entrada_de_las_herramientas_y_marca_al_agente_principal() {
        let dir = std::env::temp_dir().join(format!("pastor-search-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let main = dir.join("s.jsonl");
        let sub = dir.join("sub.jsonl");
        fs::write(
            &main,
            r#"{"type":"assistant","timestamp":"2026-10-02T10:00:00Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"rg auth.rs src/","description":"Busca usos"}}]}}"#.to_string() + "\n",
        )
        .unwrap();
        fs::write(
            &sub,
            r#"{"type":"user","isSidechain":true,"message":{"content":"Revisa auth.rs"}}"#
                .to_string()
                + "\n",
        )
        .unwrap();
        let src = Source {
            agent: "a1".into(),
            name: "api".into(),
            cwd: dir.clone(),
            transcript: main.clone(),
        };
        let q = Query::new("auth.rs", None, MAX).unwrap();
        let mut hits = Vec::new();
        search_file(&main, false, &q, &src, true, &mut hits).unwrap();
        search_file(&sub, true, &q, &src, true, &mut hits).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].role, "tool");
        assert!(hits[0].text.starts_with("Bash · Busca usos\n"));
        assert!(hits[0].snippet.contains("rg auth.rs"));
        assert_eq!(hits[1].who(), "agente principal");
    }

    #[test]
    fn trozo_alrededor_de_la_palabra() {
        let text = format!("{} aquí está auth.rs\ny sigue", "x ".repeat(100));
        let s = snippet(&text, "auth", 40);
        assert!(s.starts_with('…'));
        assert!(s.contains("auth.rs y sigue"));
        assert!(!s.contains('\n'));
        assert_eq!(snippet("corto", "nada", 40), "corto");
    }

    #[test]
    fn desde_cuando() {
        let now = Utc::now();
        let near = |t: DateTime<Utc>, d: Duration| (now - d - t).num_seconds().abs() < 5;
        assert!(near(parse_since("7d").unwrap(), Duration::days(7)));
        assert!(near(parse_since("12h").unwrap(), Duration::hours(12)));
        assert!(near(parse_since("2w").unwrap(), Duration::weeks(2)));
        assert!(parse_since("30s").is_err());
        assert!(parse_since("2026-10-01").is_ok());
        assert!(parse_since("ayer").is_err());
        // Hacia el futuro no tiene sentido.
        assert!(parse_since("-3d").is_err());
        assert!(parse_since("4000000000s").is_err());
        assert!(parse_since("").is_err());
    }
}
