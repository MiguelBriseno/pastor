//! `pastor serve`: una API HTTP para manejar los agentes desde otro
//! dispositivo (el celular). Solo se compila con el feature `serve`.
//!
//! Llama a `ops` igual que la CLI. Controlar un agente equivale a ejecutar
//! comandos en esta máquina, así que todo pide el token.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use axum::extract::{Path, Request, State as Shared};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{Kind, Status};
use crate::ops::{self, AssignTarget, Tracker};
use crate::store::{self, Agent, State, TaskStatus};

pub const DEFAULT_BIND: &str = "127.0.0.1:7433";

struct App {
    token: String,
    /// Para saber si la pantalla de un agente cambió entre dos consultas.
    tracker: Mutex<Tracker>,
}

pub fn run(bind: SocketAddr, token: Option<String>) -> Result<()> {
    let token = token.or_else(|| std::env::var("PASTOR_TOKEN").ok());
    let token = match token.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => saved_token()?,
    };
    if bind.ip().is_unspecified() {
        eprintln!(
            "Aviso: escuchando en todas las interfaces ({bind}). Cualquiera en esas redes puede \
             intentar conectarse; mejor la IP de Tailscale o 127.0.0.1."
        );
    }
    let app = Arc::new(App {
        token,
        tracker: Mutex::default(),
    });
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .with_context(|| format!("no pude escuchar en {bind}"))?;
            println!(
                "pastor serve en http://{bind} · token en {}",
                token_path().display()
            );
            axum::serve(listener, router(app))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
            Ok(())
        })
}

fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/agents", get(list_agents).post(new_agent))
        .route("/agents/{id}", delete(kill_agent))
        .route("/agents/{id}/send", post(send))
        .route("/tasks", get(list_tasks).post(add_task))
        .route("/tasks/{id}", delete(delete_task))
        .route("/tasks/{id}/assign", post(assign_task))
        .route("/tasks/{id}/done", post(task_done))
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new().nest("/api", api).with_state(app)
}

// ---------- token ----------

fn token_path() -> PathBuf {
    store::data_dir().join("serve-token")
}

/// El token guardado, o uno nuevo si no hay. `PASTOR_TOKEN` o `--token` lo
/// reemplazan sin tocar el archivo.
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

/// Ejecuta `f` (que toca tmux y el disco) fuera del hilo de tokio. Un error
/// de `ops` es casi siempre del pedido (agente que no existe, texto vacío…).
async fn blocking<T: Serialize + Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, format!("{e:#}")),
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

async fn new_agent(Json(req): Json<NewAgent>) -> Response {
    blocking(move || ops::new_agent(req.kind, Some(req.cwd), req.name, req.prompt.as_deref())).await
}

#[derive(Deserialize)]
struct Message {
    text: String,
}

async fn send(Path(id): Path<String>, Json(req): Json<Message>) -> Response {
    blocking(move || ops::send(&id, &req.text).map(|()| ok())).await
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

async fn add_task(Json(req): Json<NewTask>) -> Response {
    blocking(move || ops::add_task(req.title, req.description)).await
}

/// A un agente existente (`agent`) o a uno nuevo (`kind` y `cwd`).
#[derive(Deserialize)]
struct Assign {
    agent: Option<String>,
    kind: Option<Kind>,
    cwd: Option<PathBuf>,
}

async fn assign_task(Path(id): Path<String>, Json(req): Json<Assign>) -> Response {
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
        } => AssignTarget::New {
            kind,
            cwd: Some(cwd),
        },
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
    fn sin_token_no_hay_api() {
        assert!(get("/api/tasks", None).starts_with("HTTP/1.1 401"));
        assert!(get("/api/tasks", Some("Bearer otro")).starts_with("HTTP/1.1 401"));
        assert!(get("/api/agents", Some("secreto")).starts_with("HTTP/1.1 401"));
        // Lo que no es de la API no existe (todavía).
        assert!(get("/", None).starts_with("HTTP/1.1 404"));
    }
}
