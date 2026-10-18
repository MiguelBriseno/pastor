//! Cuánto queda de los límites de uso de las suscripciones (Claude, Codex).
//!
//! Ninguno de los dos dice cuántos tokens quedan: dan el porcentaje usado de
//! cada ventana (5 horas, semana…) y cuándo se reinicia.
//!
//! - Claude: `api.anthropic.com/api/oauth/usage` con el token de Claude Code
//!   (`~/.claude/.credentials.json`). No es una API documentada: si cambia o
//!   falla, se muestra el error y nada más.
//! - Codex: los eventos `token_count` de `~/.codex/sessions`, tan frescos
//!   como la última sesión de Codex.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::Value;

use crate::agent::Kind;

/// Cada cuánto se vuelve a preguntar.
pub const TTL: Duration = Duration::from_secs(60);
/// Desde cuándo un dato se considera viejo y se dice de cuándo es.
const STALE: Duration = Duration::from_secs(5 * 60);
/// Lo que se lee del final de una sesión de Codex buscando el último evento.
const TAIL: u64 = 1 << 20;
/// Cuántas sesiones de Codex, de la más reciente hacia atrás, se miran: sin
/// tope, alguien sin límites (con clave de API) leería todas cada `TTL`.
const SESSIONS: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// "5 h", "semana", "semana Opus"…
    pub name: String,
    pub used_pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl Window {
    pub fn left_pct(&self) -> f64 {
        (100.0 - self.used_pct).clamp(0.0, 100.0)
    }
}

/// Con `left_pct` ya calculado, para que la app no repita la cuenta.
impl Serialize for Window {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Window", 4)?;
        st.serialize_field("name", &self.name)?;
        st.serialize_field("used_pct", &self.used_pct)?;
        st.serialize_field("left_pct", &self.left_pct())?;
        st.serialize_field("resets_at", &self.resets_at)?;
        st.end()
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Usage {
    pub kind: Kind,
    pub windows: Vec<Window>,
    /// Plan, si la fuente lo dice ("plus", "free"…).
    pub plan: Option<String>,
    /// De cuándo es el dato (Codex: el último evento visto).
    pub updated_at: Option<DateTime<Utc>>,
    /// Si `updated_at` es de hace más de `STALE` (el de Codex puede ser de
    /// hace días). Lo dice el servidor para que la app no dependa de su reloj.
    pub stale: bool,
    /// Por qué no hay datos.
    pub error: Option<String>,
}

impl Usage {
    fn from(kind: Kind, r: Result<Usage>) -> Usage {
        r.unwrap_or_else(|e| Usage {
            kind,
            windows: Vec::new(),
            plan: None,
            updated_at: None,
            stale: false,
            error: Some(format!("{e:#}")),
        })
    }
}

/// Los límites leídos la última vez, para no preguntar más de una vez por `TTL`.
#[derive(Default)]
pub struct Cache {
    at: Option<Instant>,
    usage: Vec<Usage>,
}

impl Cache {
    /// Si el dato es viejo vuelve a leerlo (puede tardar unos segundos por la
    /// red). Devuelve si se leyó de nuevo.
    pub fn refresh(&mut self) -> bool {
        if self.fresh() {
            return false;
        }
        self.usage = fetch();
        self.at = Some(Instant::now());
        true
    }

    fn fresh(&self) -> bool {
        self.at.is_some_and(|t| t.elapsed() < TTL)
    }

    /// Si el dato es viejo, da la lectura por empezada y devuelve su marca:
    /// quien lo llama lee con `fetch` sin tener el candado y lo guarda con
    /// `set`, mientras las demás consultas sirven el dato de antes.
    #[cfg(any(feature = "serve", test))]
    pub fn claim(&mut self) -> Option<Instant> {
        if self.fresh() {
            return None;
        }
        let at = Instant::now();
        self.at = Some(at);
        Some(at)
    }

    /// Guarda la lectura `at`, salvo que haya empezado otra después: la más
    /// nueva no la pisa una lenta que acabó tarde.
    #[cfg(any(feature = "serve", test))]
    pub fn set(&mut self, at: Instant, usage: Vec<Usage>) {
        if self.at == Some(at) {
            self.usage = usage;
        }
    }

    /// La lectura `at` no terminó: que la siguiente consulta lo intente.
    #[cfg(any(feature = "serve", test))]
    pub fn forget(&mut self, at: Instant) {
        if self.at == Some(at) {
            self.at = None;
        }
    }

    pub fn usage(&self) -> &[Usage] {
        &self.usage
    }
}

pub fn fetch() -> Vec<Usage> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        Usage::from(Kind::Claude, claude(&home)),
        Usage::from(
            Kind::Codex,
            codex(&home.join(".codex/sessions"), Utc::now()),
        ),
    ]
}

// ---------- Claude ----------

fn claude(home: &Path) -> Result<Usage> {
    let path = home.join(".claude/.credentials.json");
    let creds: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).context("no hay sesión de Claude Code")?,
    )?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .ok_or_else(|| anyhow!("no hay sesión de Claude Code"))?;
    if oauth["expiresAt"]
        .as_i64()
        .is_some_and(|ms| ms < Utc::now().timestamp_millis())
    {
        bail!("la sesión venció; abre Claude Code para renovarla");
    }
    let mut usage = parse_claude(&get_usage(token)?)?;
    usage.plan = oauth["subscriptionType"].as_str().map(str::to_string);
    Ok(usage)
}

fn get_usage(token: &str) -> Result<Value> {
    let mut child = Command::new("curl")
        // La configuración por la entrada: así el token no se ve en `ps`.
        .args(["-fsS", "-m", "5", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("no pude ejecutar curl")?;
    let config = format!(
        "url = \"https://api.anthropic.com/api/oauth/usage\"\n\
         header = \"Authorization: Bearer {token}\"\n\
         header = \"anthropic-beta: oauth-2025-04-20\"\n"
    );
    // Si curl murió antes de leerla, su stderr explica más que el "broken pipe".
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config.as_bytes());
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("no pude consultar el uso: {}", err.trim());
    }
    serde_json::from_slice(&out.stdout).context("respuesta de uso inesperada")
}

fn parse_claude(v: &Value) -> Result<Usage> {
    let windows: Vec<Window> = [
        ("five_hour", "5 h"),
        ("seven_day", "semana"),
        ("seven_day_opus", "semana Opus"),
        ("seven_day_sonnet", "semana Sonnet"),
    ]
    .into_iter()
    .filter_map(|(key, name)| {
        let w = &v[key];
        Some(Window {
            name: name.to_string(),
            used_pct: w["utilization"].as_f64()?,
            resets_at: w["resets_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc)),
        })
    })
    .collect();
    if windows.is_empty() {
        bail!("respuesta de uso inesperada");
    }
    Ok(Usage {
        kind: Kind::Claude,
        windows,
        plan: None,
        updated_at: Some(Utc::now()),
        stale: false,
        error: None,
    })
}

// ---------- Codex ----------

fn codex(dir: &Path, now: DateTime<Utc>) -> Result<Usage> {
    let mut files = Vec::new();
    jsonl_files(dir, &mut files);
    if files.is_empty() {
        bail!("no hay sesiones de Codex");
    }
    // La más reciente con datos: una sesión recién abierta aún no los tiene.
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
        .iter()
        .take(SESSIONS)
        .find_map(|(_, path)| last_rate_limits(path))
        .map(|(at, v)| parse_codex(&v, at, now))
        .ok_or_else(|| anyhow!("las sesiones de Codex no traen límites todavía"))
}

fn jsonl_files(dir: &Path, out: &mut Vec<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            jsonl_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "jsonl") {
            if let Ok(t) = meta.modified() {
                out.push((t, path));
            }
        }
    }
}

/// El último `rate_limits` de la sesión, con la hora del evento.
fn last_rate_limits(path: &Path) -> Option<(Option<DateTime<Utc>>, Value)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter(|l| l.contains("\"rate_limits\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| {
            let limits = &v["payload"]["rate_limits"];
            // Sin ninguna ventana no dice nada: el evento de antes sí.
            let any = ["primary", "secondary"]
                .iter()
                .any(|k| limits[*k]["used_percent"].is_number());
            any.then(|| {
                let at = v["timestamp"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc));
                (at, limits.clone())
            })
        })
}

fn parse_codex(v: &Value, at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Usage {
    let windows = ["primary", "secondary"]
        .iter()
        .filter_map(|key| {
            let w = &v[key];
            let used = w["used_percent"].as_f64()?;
            let resets_at = w["resets_at"]
                .as_i64()
                .and_then(|s| Utc.timestamp_opt(s, 0).single());
            Some(Window {
                name: window_name(w["window_minutes"].as_i64()),
                // El dato es de la última sesión: si la ventana ya se
                // reinició desde entonces, está libre.
                used_pct: if resets_at.is_some_and(|r| r <= now) {
                    0.0
                } else {
                    used
                },
                resets_at: resets_at.filter(|r| *r > now),
            })
        })
        .collect();
    Usage {
        kind: Kind::Codex,
        windows,
        plan: v["plan_type"].as_str().map(str::to_string),
        updated_at: at,
        stale: at.is_some_and(|t| now - t >= chrono::Duration::from_std(STALE).unwrap_or_default()),
        error: None,
    }
}

fn window_name(minutes: Option<i64>) -> String {
    match minutes {
        Some(10_080) => "semana".to_string(),
        Some(m) if m >= 1440 && m % 1440 == 0 => format!("{} días", m / 1440),
        Some(m) if m >= 60 && m % 60 == 0 => format!("{} h", m / 60),
        Some(m) => format!("{m} min"),
        None => "ventana".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lee_el_uso_de_claude() {
        let v = json!({
            "five_hour": { "utilization": 42.0, "resets_at": "2026-10-18T21:20:00.423018+00:00" },
            "seven_day": { "utilization": 10.0, "resets_at": "2026-10-24T01:00:00+00:00" },
            "seven_day_opus": null,
        });
        let u = parse_claude(&v).unwrap();
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].name, "5 h");
        assert_eq!(u.windows[0].left_pct(), 58.0);
        assert_eq!(
            serde_json::to_value(&u.windows[0]).unwrap()["left_pct"],
            58.0
        );
        assert_eq!(
            u.windows[1].resets_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 24, 1, 0, 0).unwrap())
        );
        assert!(parse_claude(&json!({})).is_err());
    }

    #[test]
    fn lee_el_uso_de_codex_y_libera_ventanas_reiniciadas() {
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 12, 0, 0).unwrap();
        let later = now.timestamp() + 3600;
        let before = now.timestamp() - 60;
        let v = json!({
            "primary": { "used_percent": 62.0, "window_minutes": 300, "resets_at": later },
            "secondary": { "used_percent": 80.0, "window_minutes": 10080, "resets_at": before },
            "plan_type": "plus",
        });
        let u = parse_codex(&v, None, now);
        assert!(!u.stale);
        assert!(parse_codex(&v, Some(now - chrono::Duration::minutes(5)), now).stale);
        assert!(!parse_codex(&v, Some(now - chrono::Duration::minutes(4)), now).stale);
        assert_eq!(u.plan.as_deref(), Some("plus"));
        assert_eq!(u.windows[0].name, "5 h");
        assert_eq!(u.windows[0].used_pct, 62.0);
        assert_eq!(u.windows[1].name, "semana");
        assert_eq!(u.windows[1].used_pct, 0.0);
        assert_eq!(u.windows[1].resets_at, None);
    }

    #[test]
    fn una_lectura_vieja_no_pisa_una_nueva() {
        let usage = |pct| {
            vec![Usage {
                kind: Kind::Codex,
                windows: vec![Window {
                    name: "5 h".into(),
                    used_pct: pct,
                    resets_at: None,
                }],
                plan: None,
                updated_at: None,
                stale: false,
                error: None,
            }]
        };
        let mut c = Cache::default();
        let a = c.claim().unwrap();
        assert!(c.claim().is_none());
        // `a` tardó más que `TTL` y otra lectura empezó.
        c.at = Some(a - TTL);
        let b = c.claim().unwrap();
        c.set(b, usage(20.0));
        c.set(a, usage(10.0));
        assert_eq!(c.usage()[0].windows[0].used_pct, 20.0);
        // Una que no terminó deja intentarlo de nuevo.
        c.at = Some(b - TTL);
        let d = c.claim().unwrap();
        c.forget(d);
        assert!(c.claim().is_some());
    }

    #[test]
    fn nombres_de_ventana() {
        assert_eq!(window_name(Some(43_200)), "30 días");
        assert_eq!(window_name(Some(10_080)), "semana");
        assert_eq!(window_name(Some(300)), "5 h");
        assert_eq!(window_name(Some(45)), "45 min");
    }

    #[test]
    fn toma_la_ultima_sesion_con_limites() {
        let dir = std::env::temp_dir().join(format!("pastor-limits-{}", std::process::id()));
        let day = dir.join("2026/10/18");
        std::fs::create_dir_all(&day).unwrap();
        let line = |pct: f64| {
            json!({
                "timestamp": "2026-10-18T10:00:00Z",
                "type": "event_msg",
                "payload": { "type": "token_count", "rate_limits": {
                    "primary": { "used_percent": pct, "window_minutes": 300 }
                }}
            })
            .to_string()
        };
        std::fs::write(
            day.join("a.jsonl"),
            // El último trae `rate_limits` pero sin ventanas: no cuenta.
            format!(
                "{}\n{}\n{}\n",
                line(10.0),
                line(20.0),
                r#"{"payload":{"type":"token_count","rate_limits":{"primary":null,"secondary":null}}}"#
            ),
        )
        .unwrap();
        // Más reciente pero sin límites todavía.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(day.join("b.jsonl"), "{\"type\":\"session_meta\"}\n").unwrap();
        let u = codex(&dir, Utc::now()).unwrap();
        assert_eq!(u.windows[0].used_pct, 20.0);
        assert!(u.updated_at.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(codex(&dir, Utc::now()).is_err());
    }
}
