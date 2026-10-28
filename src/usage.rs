//! Tokens y costo de los agentes de Claude Code, leídos de sus transcripciones.
//!
//! Cada respuesta de Claude Code queda en la transcripción con su `usage`:
//! tokens de entrada, de salida y de la caché. pastor lee lo nuevo al final de
//! cada turno (y antes de terminar o reasignar un agente) y lo anota en el
//! historial como un evento `usage`, con la tarea que el agente tenía. Así el
//! uso queda aunque la transcripción se borre o el agente ya no exista.
//!
//! El costo es lo que costarían esos tokens con los precios de la API de
//! Anthropic: con una suscripción no se paga por token, pero da la medida de
//! lo que gasta cada agente y cada tarea. Codex y opencode no dejan una
//! transcripción que pastor sepa leer: de ellos no hay uso.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::ops::AddAssign;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::history::{self, Event};
use crate::store::{self, data_dir, write_atomic, Agent, State};

/// Tokens de una o varias respuestas.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    /// Escritos en la caché por 5 minutos (cuestan 1.25 veces la entrada).
    #[serde(default)]
    pub cache_write_5m: u64,
    /// Escritos en la caché por una hora (cuestan el doble de la entrada).
    #[serde(default)]
    pub cache_write_1h: u64,
    #[serde(default)]
    pub cache_read: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Los de un `usage` de la API. Sin el desglose de la caché por duración
    /// (versiones viejas), toda la escritura cuenta como de 5 minutos.
    fn from_usage(u: &Value) -> Tokens {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let write = n(&u["cache_creation_input_tokens"]);
        let split = &u["cache_creation"];
        // El de una hora del desglose; el resto de lo escrito, de 5 minutos:
        // así no se pierde nada si el desglose viene incompleto.
        let w1h = n(&split["ephemeral_1h_input_tokens"]).min(write);
        let w5 = if split.is_object() {
            n(&split["ephemeral_5m_input_tokens"]).max(write - w1h)
        } else {
            write
        };
        Tokens {
            input: n(&u["input_tokens"]),
            output: n(&u["output_tokens"]),
            cache_write_5m: w5,
            cache_write_1h: w1h,
            cache_read: n(&u["cache_read_input_tokens"]),
        }
    }
}

impl AddAssign<&Tokens> for Tokens {
    fn add_assign(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write_5m += o.cache_write_5m;
        self.cache_write_1h += o.cache_write_1h;
        self.cache_read += o.cache_read;
    }
}

/// Precios de la API en dólares por millón de tokens.
struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
}

/// Por modelo, del nombre más específico al más general: `opus-4` debe ir
/// después de `opus-4-5` para no tomarse sus precios. Haiku 5.5 cobra más
/// pasados 100K tokens de entrada; aquí va siempre el precio de abajo.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("fable-5-1", 10.0, 50.0, 0.25),
    ("mythos-5-1", 10.0, 50.0, 0.25),
    ("fable-5", 10.0, 50.0, 1.0),
    ("mythos-5", 10.0, 50.0, 1.0),
    ("opus-5-5", 4.0, 20.0, 0.20),
    ("opus-5", 5.0, 25.0, 0.50),
    ("opus-4-8", 5.0, 25.0, 0.50),
    ("opus-4-7", 5.0, 25.0, 0.50),
    ("opus-4-6", 5.0, 25.0, 0.50),
    ("opus-4-5", 5.0, 25.0, 0.50),
    ("opus-4", 15.0, 75.0, 1.50),
    ("sonnet-5-5", 2.0, 10.0, 0.20),
    ("sonnet-5", 2.0, 10.0, 0.20),
    ("sonnet-4", 3.0, 15.0, 0.30),
    ("3-7-sonnet", 3.0, 15.0, 0.30),
    ("3-5-sonnet", 3.0, 15.0, 0.30),
    ("haiku-5-5", 0.10, 0.50, 0.01),
    ("haiku-4-5", 1.0, 5.0, 0.10),
    ("3-5-haiku", 0.80, 4.0, 0.08),
];

fn price(model: &str) -> Option<Price> {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    // `opus-5` no es `opus-5-5`: después del nombre viene el final o un `-`
    // (la fecha de las versiones viejas, `-20250514`, o `[1m]`).
    PRICES
        .iter()
        .find(|(name, ..)| {
            m.strip_prefix(name)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '[', '@']))
        })
        .map(|&(_, input, output, cache_read)| Price {
            input,
            output,
            cache_read,
        })
}

/// Lo que costarían `t` tokens de `model` con los precios de la API, en
/// dólares. `None` si no se conoce el modelo.
pub fn cost(model: &str, t: &Tokens) -> Option<f64> {
    let p = price(model)?;
    let dollars = t.input as f64 * p.input
        + t.output as f64 * p.output
        + t.cache_write_5m as f64 * p.input * 1.25
        + t.cache_write_1h as f64 * p.input * 2.0
        + t.cache_read as f64 * p.cache_read;
    Some(dollars / 1_000_000.0)
}

/// Tokens y costo acumulados.
#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Spend {
    pub tokens: Tokens,
    /// En dólares, de los modelos con precio conocido.
    pub cost: f64,
    /// Si parte de los tokens son de un modelo sin precio conocido: el
    /// costo se queda corto.
    pub unpriced: bool,
}

impl Spend {
    pub fn add(&mut self, model: &str, t: &Tokens) {
        self.tokens += t;
        match cost(model, t) {
            Some(c) => self.cost += c,
            None => self.unpriced |= !t.is_empty(),
        }
    }

    pub fn merge(&mut self, o: &Spend) {
        self.tokens += &o.tokens;
        self.cost += o.cost;
        self.unpriced |= o.unpriced;
    }

    /// `1.2M tokens · ≈ $3.40`, para mostrar en una línea.
    pub fn short(&self) -> String {
        format!(
            "{} tokens · {}",
            tokens(self.tokens.total()),
            self.cost_label()
        )
    }

    /// `≈ $3.40`, con un `+` si hay tokens sin precio conocido.
    pub fn cost_label(&self) -> String {
        let plus = if self.unpriced { "+" } else { "" };
        format!("≈ {}{plus}", dollars(self.cost))
    }
}

/// `850`, `12.3k`, `1.2M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

/// `$0.42`, `$12.30`, `$1,234`.
pub fn dollars(d: f64) -> String {
    if d >= 1000.0 {
        let n = d.round() as u64;
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        format!("${out}")
    } else {
        format!("${d:.2}")
    }
}

// ---------- lectura de las transcripciones ----------

/// Hasta dónde se leyó cada transcripción de un agente, en
/// `usage/<id>.json`. Sobrevive a `/clear` (otra transcripción) y a
/// restaurar el agente.
#[derive(Serialize, Deserialize, Default)]
struct Offsets {
    #[serde(default)]
    files: BTreeMap<PathBuf, Cursor>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Cursor {
    offset: u64,
    /// La última respuesta contada: Claude Code escribe una línea por bloque
    /// de la respuesta, todas con el mismo `usage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<String>,
}

fn dir() -> PathBuf {
    data_dir().join("usage")
}

fn offsets_path(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
}

/// Las transcripciones de los subagentes de una sesión: Claude Code las
/// guarda en `<sesión>/subagents/*.jsonl`, junto a la del hilo principal.
pub fn subagent_files(main: &Path) -> Vec<PathBuf> {
    let Some(stem) = main.file_stem() else {
        return Vec::new();
    };
    let sub = main.with_file_name(stem).join("subagents");
    let Ok(entries) = fs::read_dir(sub) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect()
}

/// Si `p` es la transcripción de un subagente.
fn is_subagent(p: &Path) -> bool {
    p.parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "subagents")
}

/// Lo nuevo de una transcripción desde `r`, por modelo. Solo cuenta lo
/// escrito desde `since` (cuando se lanzó el agente): una conversación vieja
/// retomada con `/resume` ya se pagó antes. Deja `r` donde terminó la última
/// línea completa.
fn read_new(path: &Path, r: &mut Cursor, since: DateTime<Utc>) -> Result<HashMap<String, Tokens>> {
    let mut out: HashMap<String, Tokens> = HashMap::new();
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    // Se reescribió más corta: no hay forma de saber qué es nuevo.
    if len < r.offset {
        r.offset = len;
        return Ok(out);
    }
    f.seek(SeekFrom::Start(r.offset))?;
    let mut buf = Vec::new();
    f.take(len - r.offset).read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(out);
    };
    for line in String::from_utf8_lossy(&buf[..=end]).lines() {
        // Sin armar el JSON de las líneas que no son respuestas.
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let m = &v["message"];
        if v["type"] != "assistant" || !m["usage"].is_object() {
            continue;
        }
        let id = m["id"].as_str().map(String::from);
        if id.is_some() && id == r.last {
            continue;
        }
        r.last = id;
        let old = v["timestamp"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t < since);
        let model = m["model"].as_str().unwrap_or("");
        // `<synthetic>`: mensajes que Claude Code arma sin llamar a la API.
        if old || model.starts_with('<') {
            continue;
        }
        *out.entry(model.to_string()).or_default() += &Tokens::from_usage(&m["usage"]);
    }
    r.offset += end as u64 + 1;
    Ok(out)
}

/// Lee lo nuevo de las transcripciones de `agent` y lo anota en el
/// historial con la tarea que tiene ahora. `current` es la transcripción en
/// la que va, si se sabe (la trae el hook); si no, la del último hook. Solo
/// para agentes de Claude Code de esta máquina; con los demás no hace nada.
pub fn flush(agent: &Agent, current: Option<PathBuf>) -> Result<()> {
    let id = agent.id.as_str();
    let current = current.or_else(|| crate::transcript::path_for(id));
    // Ya con el candado: si lo quitaron mientras (`pastor kill` lee lo suyo
    // y luego borra hasta dónde leyó), empezar de cero lo contaría dos veces.
    let alive = || State::load().is_ok_and(|s| s.agents.iter().any(|a| a.id == id));
    for (model, tokens) in read_agent(&dir(), id, agent.created_at, current, alive)? {
        history::record(Event::Usage {
            agent: id.to_string(),
            task: agent.task.clone(),
            model,
            tokens,
        });
    }
    Ok(())
}

/// Lo nuevo de todas las transcripciones conocidas del agente `id` (más
/// `current` y las de sus subagentes), por modelo, y anota hasta dónde leyó.
/// Con el candado tomado, nada si `alive` dice que el agente ya no está.
fn read_agent(
    dir: &Path,
    id: &str,
    since: DateTime<Utc>,
    current: Option<PathBuf>,
    alive: impl FnOnce() -> bool,
) -> Result<Vec<(String, Tokens)>> {
    fs::create_dir_all(dir)?;
    // Dos a la vez (el fin del turno y `pastor kill`) contarían lo mismo dos veces.
    let _lock = store::lock(&dir.join(format!("{id}.lock")))?;
    if !alive() {
        return Ok(Vec::new());
    }
    let path = dir.join(format!("{id}.json"));
    let mut offsets: Offsets = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if current.is_none() && offsets.files.is_empty() {
        return Ok(Vec::new());
    }
    offsets.files.extend(
        current
            .into_iter()
            .map(|p| (p, Cursor::default()))
            .filter(|(p, _)| !offsets.files.contains_key(p))
            .collect::<Vec<_>>(),
    );
    let mains: Vec<PathBuf> = offsets
        .files
        .keys()
        .filter(|p| !is_subagent(p))
        .cloned()
        .collect();
    for sub in mains.iter().flat_map(|m| subagent_files(m)) {
        offsets.files.entry(sub).or_default();
    }
    let mut by_model: HashMap<String, Tokens> = HashMap::new();
    for (p, r) in offsets.files.iter_mut() {
        // Una que ya no existe (la borraron) se queda como está.
        if let Ok(new) = read_new(p, r, since) {
            for (m, t) in new {
                *by_model.entry(m).or_default() += &t;
            }
        }
    }
    // Primero dónde se quedó y después el historial: si algo falla entre
    // medias, mejor contar de menos que dos veces.
    write_atomic(&path, &serde_json::to_vec(&offsets)?)?;
    let mut models: Vec<_> = by_model
        .into_iter()
        .filter(|(_, t)| !t.is_empty())
        .collect();
    models.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(models)
}

/// Como `flush`, pero sin fallar: el uso es un extra.
pub fn flush_quietly(agent: &Agent, current: Option<PathBuf>) {
    if let Err(e) = flush(agent, current) {
        crate::hooks::log_error("usage", &e);
    }
}

/// Olvida hasta dónde se leyó: el agente ya no existe. El `.lock` se queda,
/// como el de los hooks: otro proceso puede estar esperándolo, y si se
/// borrara, el siguiente bloquearía otro archivo. Lo quita `prune`.
pub fn forget(id: &str) {
    let _ = fs::remove_file(offsets_path(id));
}

/// Borra lo de los agentes que ya no están en el estado.
pub fn prune(state: &State) {
    let Ok(entries) = fs::read_dir(dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json").or_else(|| n.strip_suffix(".lock")))
        else {
            continue;
        };
        if !state.agents.iter().any(|a| a.id == id) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn precios_por_modelo() {
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            ..Tokens::default()
        };
        assert_eq!(cost("claude-opus-5-5", &t), Some(24.0));
        assert_eq!(cost("claude-opus-5", &t), Some(30.0));
        assert_eq!(cost("claude-opus-4-1-20250805", &t), Some(90.0));
        assert_eq!(cost("claude-opus-4-5-20251101", &t), Some(30.0));
        assert_eq!(cost("claude-sonnet-4-5-20250929", &t), Some(18.0));
        assert_eq!(cost("claude-sonnet-5-5[1m]", &t), Some(12.0));
        assert_eq!(cost("claude-fable-5-1", &t), Some(60.0));
        assert_eq!(cost("claude-3-5-haiku-20241022", &t), Some(4.8));
        assert_eq!(cost("gpt-5", &t), None);
        // La caché: escribir por una hora cuesta el doble; leer, su precio.
        let cache = Tokens {
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            cache_read: 1_000_000,
            ..Tokens::default()
        };
        assert_eq!(cost("claude-opus-5-5", &cache), Some(5.0 + 8.0 + 0.2));
    }

    #[test]
    fn formatos_cortos() {
        assert_eq!(tokens(850), "850");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(1_234_567), "1.2M");
        assert_eq!(dollars(0.4249), "$0.42");
        assert_eq!(dollars(1234.4), "$1,234");
        let mut s = Spend::default();
        s.add(
            "modelo-raro",
            &Tokens {
                output: 5,
                ..Tokens::default()
            },
        );
        assert_eq!(s.cost_label(), "≈ $0.00+");
    }

    fn assistant(id: &str, model: &str, ts: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"{model}","content":[],"usage":{{"input_tokens":1,"output_tokens":{output},"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"cache_creation":{{"ephemeral_5m_input_tokens":4,"ephemeral_1h_input_tokens":6}}}}}}}}"#
        )
    }

    #[test]
    fn cuenta_cada_respuesta_una_vez_y_solo_lo_nuevo() {
        let root = std::env::temp_dir().join(format!("pastor-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let state_dir = root.join("usage");
        let main = root.join("s1.jsonl");
        let sub = root.join("s1").join("subagents").join("agent-x.jsonl");
        fs::create_dir_all(sub.parent().unwrap()).unwrap();
        let since: DateTime<Utc> = "2026-10-01T00:00:00Z".parse().unwrap();

        let mut f = File::create(&main).unwrap();
        // De antes de lanzar el agente (un `/resume`): no cuenta.
        writeln!(
            f,
            "{}",
            assistant("m0", "claude-opus-5-5", "2026-09-30T10:00:00Z", 999)
        )
        .unwrap();
        // Dos líneas de la misma respuesta: cuenta una vez.
        writeln!(
            f,
            "{}",
            assistant("m1", "claude-opus-5-5", "2026-10-02T10:00:00Z", 50)
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            assistant("m1", "claude-opus-5-5", "2026-10-02T10:00:00Z", 50)
        )
        .unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hola"}}}}"#).unwrap();
        // A medio escribir: espera.
        write!(
            f,
            "{}",
            &assistant("m2", "claude-opus-5-5", "2026-10-02T10:01:00Z", 7)[..30]
        )
        .unwrap();
        f.flush().unwrap();
        fs::write(
            &sub,
            format!(
                "{}\n",
                assistant("s1", "claude-haiku-5-5", "2026-10-02T10:00:30Z", 3)
            ),
        )
        .unwrap();

        let read = |r: &mut Cursor| read_new(&main, r, since).unwrap();
        let mut r = Cursor::default();
        let got = read(&mut r);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got["claude-opus-5-5"],
            Tokens {
                input: 1,
                output: 50,
                cache_write_5m: 4,
                cache_write_1h: 6,
                cache_read: 100
            }
        );
        assert!(read(&mut r).is_empty());
        writeln!(
            f,
            "{}",
            &assistant("m2", "claude-opus-5-5", "2026-10-02T10:01:00Z", 7)[30..]
        )
        .unwrap();
        f.flush().unwrap();
        assert_eq!(read(&mut r)["claude-opus-5-5"].output, 7);

        // Con todo el agente: la del subagente también, y lo ya leído no
        // vuelve a contar.
        let all = |dir: &Path| read_agent(dir, "a1", since, Some(main.clone()), || true).unwrap();
        let got = all(&state_dir);
        let models: Vec<_> = got.iter().map(|(m, t)| (m.as_str(), t.output)).collect();
        assert_eq!(models, [("claude-haiku-5-5", 3), ("claude-opus-5-5", 57)]);
        assert!(all(&state_dir).is_empty());
        // Si el agente ya no está, no lee nada aunque haya algo nuevo.
        writeln!(
            f,
            "{}",
            assistant("m3", "claude-opus-5-5", "2026-10-02T10:02:00Z", 9)
        )
        .unwrap();
        f.flush().unwrap();
        let gone = read_agent(&state_dir, "a1", since, Some(main.clone()), || false).unwrap();
        assert!(gone.is_empty());
        assert_eq!(all(&state_dir).len(), 1);
        let offsets: Offsets =
            serde_json::from_slice(&fs::read(state_dir.join("a1.json")).unwrap()).unwrap();
        assert!(offsets.files.contains_key(&sub));
        assert!(offsets.files.values().all(|r| r.offset > 0));
        fs::remove_dir_all(&root).unwrap();
    }
}
