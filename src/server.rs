//! `pastor serve`: una API HTTP para manejar los agentes desde otro
//! dispositivo (el celular). Solo se compila con el feature `serve`.
//!
//! Llama a `ops` igual que la CLI. Controlar un agente equivale a ejecutar
//! comandos en esta máquina, así que todo pide el token.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use axum::extract::{FromRequest, Path, Query, Request, State as Shared};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{Kind, Status};
use crate::ops::{self, AssignTarget, Tracker};
use crate::store::{self, Agent, State, TaskStatus};
use crate::transcript;

pub const DEFAULT_BIND: &str = "127.0.0.1:7433";

struct App {
    token: String,
    /// Para saber si la pantalla de un agente cambió entre dos consultas.
    tracker: Mutex<Tracker>,
    /// La conversación leída de cada agente, para leer solo lo nuevo.
    transcripts: Mutex<HashMap<String, Arc<Mutex<transcript::Reader>>>>,
}

/// El token sale de `PASTOR_TOKEN` o del archivo, nunca de un argumento:
/// los argumentos se ven en `ps`.
pub fn run(bind: SocketAddr) -> Result<()> {
    let (token, from) = match std::env::var("PASTOR_TOKEN").ok().filter(|t| !t.is_empty()) {
        Some(t) => (t, "PASTOR_TOKEN".to_string()),
        None => (saved_token()?, token_path().display().to_string()),
    };
    if !private(bind.ip()) {
        eprintln!(
            "Aviso: escuchando en {bind} sin cifrar. Cualquiera en esa red puede ver el token; \
             mejor 127.0.0.1 o la IP de Tailscale."
        );
    }
    let app = Arc::new(App {
        token,
        tracker: Mutex::default(),
        transcripts: Mutex::default(),
    });
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .with_context(|| format!("no pude escuchar en {bind}"))?;
            println!("pastor serve en http://{bind} · token en {from}");
            axum::serve(listener, router(app))
                .with_graceful_shutdown(shutdown())
                .await?;
            Ok(())
        })
}

fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/agents", get(list_agents).post(new_agent))
        .route("/agents/{id}", delete(kill_agent))
        .route("/agents/{id}/send", post(send))
        .route("/agents/{id}/keys", post(keys))
        .route("/agents/{id}/screen", get(screen))
        .route("/agents/{id}/conversation", get(conversation))
        .route("/tasks", get(list_tasks).post(add_task))
        .route("/tasks/{id}", delete(delete_task))
        .route("/tasks/{id}/assign", post(assign_task))
        .route("/tasks/{id}/done", post(task_done))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "no existe esa ruta") })
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .nest("/api", api)
        .route("/", get(|| page("index.html")))
        .route("/{file}", get(|Path(file): Path<String>| page(file)))
        .with_state(app)
}

/// La PWA de `web/`, metida en el binario. No pide token: la página no
/// sirve de nada sin él, y así puede pedirlo.
const WEB: &[(&str, &str, &[u8])] = &[
    (
        "index.html",
        "text/html; charset=utf-8",
        include_bytes!("../web/index.html"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/app.js"),
    ),
    (
        "style.css",
        "text/css; charset=utf-8",
        include_bytes!("../web/style.css"),
    ),
    (
        "sw.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/sw.js"),
    ),
    (
        "manifest.webmanifest",
        "application/manifest+json",
        include_bytes!("../web/manifest.webmanifest"),
    ),
    (
        "icon-192.png",
        "image/png",
        include_bytes!("../web/icon-192.png"),
    ),
    (
        "icon-512.png",
        "image/png",
        include_bytes!("../web/icon-512.png"),
    ),
];

async fn page(file: impl AsRef<str>) -> Response {
    match WEB.iter().find(|(name, ..)| *name == file.as_ref()) {
        // `no-cache`: que el navegador pregunte siempre y vea un pastor nuevo.
        Some((_, mime, body)) => (
            [
                (header::CONTENT_TYPE, *mime),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *body,
        )
            .into_response(),
        None => error(StatusCode::NOT_FOUND, "no existe esa ruta"),
    }
}

/// Ctrl-C o SIGTERM (`kill`, systemd): termina lo que está en curso, para no
/// dejar una asignación a medias.
async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let term = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = term => {}
    }
}

/// Si en `ip` el tráfico no sale de la máquina o va por Tailscale, que ya lo
/// cifra (100.64.0.0/10 y fd7a:115c:a1e0::/48).
fn private(ip: IpAddr) -> bool {
    match ip {
        _ if ip.is_loopback() => true,
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

// ---------- token ----------

fn token_path() -> PathBuf {
    store::data_dir().join("serve-token")
}

/// El token guardado, o uno nuevo si no hay. `PASTOR_TOKEN` lo reemplaza sin
/// tocar el archivo.
fn saved_token() -> Result<String> {
    let path = token_path();
    if let Ok(t) = std::fs::read_to_string(&path) {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    let token = new_token()?;
    store::ensure_dirs()?;
    store::write_atomic_mode(&path, token.as_bytes(), Some(0o600))?;
    Ok(token)
}

fn new_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("no pude generar el token")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compara sin cortar en el primer byte distinto, para no dar pistas por el
/// tiempo de respuesta.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(Shared(app): Shared<Arc<App>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.as_bytes(), app.token.as_bytes()));
    if !ok {
        return error(StatusCode::UNAUTHORIZED, "falta el token o no es válido");
    }
    next.run(req).await
}

// ---------- respuestas ----------

fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

/// Como `Json`, pero un cuerpo que no se entiende también responde con
/// `{"error": ...}`.
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Response> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Body(v)),
            Err(e) => Err(error(e.status(), e.body_text())),
        }
    }
}

/// `cwd` llega del celular: relativo o con `~` se resolvería contra el
/// directorio desde el que se lanzó el servidor.
fn not_absolute(cwd: &std::path::Path) -> Option<Response> {
    (!cwd.is_absolute()).then(|| {
        error(
            StatusCode::BAD_REQUEST,
            format!("`cwd` debe ser una ruta absoluta: {}", cwd.display()),
        )
    })
}

/// Ejecuta `f` (que toca tmux y el disco) fuera del hilo de tokio. Un error
/// de `ops` es casi siempre del pedido (agente que no existe, texto vacío…).
async fn blocking<T: Serialize + Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            // Los de `find_agent` y `find_task`.
            let code = if msg.starts_with("no existe") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error(code, msg)
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn ok() -> Value {
    json!({ "ok": true })
}

/// El estado con un nombre estable para los clientes; `status_label` es el
/// texto que muestra la TUI.
fn status_key(s: Status) -> &'static str {
    match s {
        Status::Starting => "starting",
        Status::Working => "working",
        Status::Blocked => "blocked",
        Status::Idle => "idle",
        Status::Dead => "dead",
    }
}

#[derive(Serialize)]
struct AgentView {
    #[serde(flatten)]
    agent: Agent,
    status: &'static str,
    status_label: &'static str,
}

// ---------- agentes ----------

async fn list_agents(Shared(app): Shared<Arc<App>>) -> Response {
    blocking(move || {
        let state = State::load()?;
        let snaps = {
            let mut tracker = app.tracker.lock().unwrap_or_else(PoisonError::into_inner);
            ops::observe(&state, &mut tracker)?
        };
        Ok(state
            .agents
            .into_iter()
            .map(|a| {
                let st = snaps.get(&a.id).map_or(Status::Dead, |s| s.status);
                AgentView {
                    agent: a,
                    status: status_key(st),
                    status_label: st.label(),
                }
            })
            .collect::<Vec<_>>())
    })
    .await
}

#[derive(Deserialize)]
struct NewAgent {
    kind: Kind,
    /// Obligatorio: el directorio actual del servidor no le dice nada al
    /// celular.
    cwd: PathBuf,
    name: Option<String>,
    prompt: Option<String>,
}

async fn new_agent(Body(req): Body<NewAgent>) -> Response {
    if let Some(r) = not_absolute(&req.cwd) {
        return r;
    }
    blocking(move || ops::new_agent(req.kind, Some(req.cwd), req.name, req.prompt.as_deref())).await
}

#[derive(Deserialize)]
struct Message {
    text: String,
}

async fn send(Path(id): Path<String>, Body(req): Body<Message>) -> Response {
    blocking(move || ops::send(&id, &req.text).map(|()| ok())).await
}

#[derive(Deserialize)]
struct Keys {
    keys: Vec<String>,
}

async fn keys(Path(id): Path<String>, Body(req): Body<Keys>) -> Response {
    blocking(move || ops::send_keys(&id, &req.keys).map(|()| ok())).await
}

/// Con `cols` y `rows`, el tamaño de la pantalla de quien mira.
#[derive(Deserialize)]
struct Size {
    cols: Option<u16>,
    rows: Option<u16>,
}

/// Con `?cols=…&rows=…`, antes ajusta el agente a ese tamaño para que dibuje
/// líneas que quepan en el celular.
async fn screen(Path(id): Path<String>, Query(size): Query<Size>) -> Response {
    blocking(move || {
        if let Size {
            cols: Some(cols),
            rows: Some(rows),
        } = size
        {
            // Si cambió, una pausa para que el agente alcance a redibujarse.
            // Si falla (el agente ya salió), `screen` da el error claro.
            if matches!(ops::fit(&id, cols, rows), Ok(true)) {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        ops::screen(&id).map(|text| json!({ "text": text }))
    })
    .await
}

/// Qué parte de la conversación se pide. Sin nada, los últimos mensajes;
/// con `after`, los que siguen (lo nuevo); con `before`, los anteriores.
/// Ambos valen solo para la `session` que devolvió la consulta anterior.
#[derive(Deserialize)]
struct Since {
    session: Option<String>,
    after: Option<usize>,
    before: Option<usize>,
}

/// Cuántos mensajes van como máximo en una respuesta.
const PAGE: usize = 300;

/// La conversación de un agente de Claude, de su transcripción, por partes.
/// `available: false` si aún no se sabe dónde está o ya no existe (Codex y
/// OpenCode no la dan, y Claude Code la da en su primer hook).
async fn conversation(
    Shared(app): Shared<Arc<App>>,
    Path(id): Path<String>,
    Query(since): Query<Since>,
) -> Response {
    blocking(move || {
        let state = State::load()?;
        let agent = state.find_agent(&id)?;
        let Some(path) = transcript::path_for(&agent.id).filter(|p| p.is_file()) else {
            return Ok(json!({ "available": false }));
        };
        // Uno por agente: leer la primera vez una transcripción larga no
        // detiene las consultas de los demás.
        let reader = {
            let mut readers = app
                .transcripts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            readers.retain(|k, _| state.agents.iter().any(|a| &a.id == k));
            readers.entry(agent.id.clone()).or_default().clone()
        };
        let mut r = reader.lock().unwrap_or_else(PoisonError::into_inner);
        r.update(&path)?;
        // Si la transcripción se reescribió, lo que tiene el cliente ya no
        // vale: con otra `session`, la app empieza de nuevo.
        let session = format!(
            "{}.{}",
            path.file_stem().unwrap_or_default().to_string_lossy(),
            r.resets
        );
        let total = r.messages.len();
        let same = since.session.as_deref() == Some(session.as_str());
        let (from, to) = match since {
            Since { after: Some(n), .. } if same => (n.min(total), total),
            Since {
                before: Some(n), ..
            } if same => (n.min(total).saturating_sub(PAGE), n.min(total)),
            _ => (total.saturating_sub(PAGE), total),
        };
        let to = to.min(from + PAGE);
        Ok(json!({
            "available": true,
            "session": session,
            "from": from,
            "to": to,
            "total": total,
            "messages": &r.messages[from..to],
        }))
    })
    .await
}

async fn kill_agent(Path(id): Path<String>) -> Response {
    blocking(move || ops::kill_agent(&id).map(|()| ok())).await
}

// ---------- tareas ----------

async fn list_tasks() -> Response {
    blocking(|| Ok(State::load()?.tasks)).await
}

#[derive(Deserialize)]
struct NewTask {
    title: String,
    #[serde(default)]
    description: String,
}

async fn add_task(Body(req): Body<NewTask>) -> Response {
    blocking(move || ops::add_task(req.title, req.description)).await
}

/// A un agente existente (`agent`) o a uno nuevo (`kind` y `cwd`).
#[derive(Deserialize)]
struct Assign {
    agent: Option<String>,
    kind: Option<Kind>,
    cwd: Option<PathBuf>,
}

async fn assign_task(Path(id): Path<String>, Body(req): Body<Assign>) -> Response {
    let target = match req {
        Assign {
            agent: Some(a),
            kind: None,
            cwd: None,
        } => AssignTarget::Existing(a),
        Assign {
            agent: None,
            kind: Some(kind),
            cwd: Some(cwd),
        } => {
            if let Some(r) = not_absolute(&cwd) {
                return r;
            }
            AssignTarget::New {
                kind,
                cwd: Some(cwd),
            }
        }
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "indica `agent`, o `kind` y `cwd` para uno nuevo",
            )
        }
    };
    blocking(move || ops::assign_task(&id, target)).await
}

async fn task_done(Path(id): Path<String>) -> Response {
    blocking(move || ops::set_task_status(&id, TaskStatus::Done).map(|()| ok())).await
}

async fn delete_task(Path(id): Path<String>) -> Response {
    blocking(move || ops::delete_task(&id).map(|()| ok())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn compara_tokens() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }

    #[test]
    fn redes_sin_aviso() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(private(ip("127.0.0.1")));
        assert!(private(ip("::1")));
        assert!(private(ip("100.101.102.103")));
        assert!(private(ip("fd7a:115c:a1e0::1")));
        assert!(!private(ip("0.0.0.0")));
        assert!(!private(ip("192.168.1.20")));
        assert!(!private(ip("100.128.0.1")));
    }

    #[test]
    fn el_token_nuevo_es_largo_y_distinto() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// Levanta el router en un puerto libre y hace un GET con `auth`.
    fn get(path: &str, auth: Option<&str>) -> String {
        let app = Arc::new(App {
            token: "secreto".into(),
            tracker: Mutex::default(),
            transcripts: Mutex::default(),
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(async move { axum::serve(listener, router(app)).await });
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        let auth = auth.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn la_pagina_no_pide_token() {
        let r = get("/", None);
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("text/html") && r.contains("app.js"), "{r}");
        assert!(get("/sw.js", None).contains("text/javascript"));
        assert!(get("/manifest.webmanifest", None).starts_with("HTTP/1.1 200"));
        assert!(get("/nada.js", None).starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn sin_token_no_hay_api() {
        assert!(get("/api/tasks", None).starts_with("HTTP/1.1 401"));
        assert!(get("/api/tasks", Some("Bearer otro")).starts_with("HTTP/1.1 401"));
        assert!(get("/api/agents", Some("secreto")).starts_with("HTTP/1.1 401"));
        let r = get("/api/nada", Some("Bearer secreto"));
        assert!(
            r.starts_with("HTTP/1.1 404") && r.contains(r#"{"error":"#),
            "{r}"
        );
    }
}
