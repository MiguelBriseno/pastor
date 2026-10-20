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
use axum::extract::ws::{self, CloseFrame, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State as Shared};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{Kind, Status};
use crate::history;
use crate::limits;
use crate::ops::{self, AssignTarget, Place, Tracker};
use crate::pty::Pty;
use crate::store::{self, Agent, State, TaskStatus};
use crate::tmux;
use crate::transcript;

pub const DEFAULT_BIND: &str = "127.0.0.1:7433";

struct App {
    token: String,
    /// Para saber si la pantalla de un agente cambió entre dos consultas.
    tracker: Mutex<Tracker>,
    /// La conversación leída de cada agente, para leer solo lo nuevo.
    transcripts: Mutex<HashMap<String, Arc<Mutex<transcript::Reader>>>>,
    /// El historial leído, para leer solo las líneas nuevas.
    history: Mutex<history::Tail>,
    /// Los límites de uso leídos, para no preguntar en cada consulta.
    limits: Mutex<limits::Cache>,
    /// Pasa a `true` al apagarse, para que las terminales abiertas intenten
    /// despedirse. axum no espera a los WebSocket: si no alcanzan, al soltar
    /// el runtime el `Drop` de cada `Pty` igual cierra su tmux.
    closing: tokio::sync::watch::Sender<bool>,
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
    // Los avisos de los agentes, para que lleguen también con la TUI cerrada.
    std::thread::Builder::new()
        .name("avisos".into())
        .spawn(crate::avisos::watch_forever)
        .context("no pude lanzar el hilo de los avisos")?;
    let app = Arc::new(App {
        token,
        tracker: Mutex::default(),
        transcripts: Mutex::default(),
        history: Mutex::default(),
        limits: Mutex::default(),
        closing: tokio::sync::watch::Sender::new(false),
    });
    let closing = app.closing.clone();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .with_context(|| format!("no pude escuchar en {bind}"))?;
            println!("pastor serve en http://{bind} · token en {from}");
            axum::serve(listener, router(app))
                .with_graceful_shutdown(async move {
                    shutdown().await;
                    closing.send_replace(true);
                })
                .await?;
            Ok(())
        })
}

fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/agents", get(list_agents).post(new_agent))
        .route("/agents/{id}", delete(kill_agent))
        .route("/agents/{id}/restore", post(restore_agent))
        .route("/agents/{id}/send", post(send))
        .route("/agents/{id}/keys", post(keys))
        .route("/agents/{id}/screen", get(screen))
        .route("/agents/{id}/conversation", get(conversation))
        .route("/agents/{id}/attach", get(attach))
        .route("/tasks", get(list_tasks).post(add_task))
        .route("/tasks/{id}", delete(delete_task))
        .route("/tasks/{id}/assign", post(assign_task))
        .route("/tasks/{id}/done", post(task_done))
        .route("/tasks/{id}/approve", post(approve_task))
        .route("/tasks/{id}/reject", post(reject_task))
        .route("/stats", get(stats))
        .route("/limits", get(limits))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "no existe esa ruta") })
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .nest("/api", api)
        .route("/", get(|| page("index.html")))
        .route("/{file}", get(|Path(file): Path<String>| page(file)))
        .layer(middleware::from_fn(headers))
        .with_state(app)
}

/// Defensa en profundidad: con la CSP un XSS no podría mandar el token a
/// otro lado, nadie puede meter la app en un marco, y un enlace de tarea no
/// le dice al sitio externo de qué máquina viene.
async fn headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            // Estilos inline: xterm.js arma los suyos con elementos `<style>`.
            // Con imágenes, letras y conexiones solo del mismo origen, un
            // estilo inyectado no tiene adónde mandar nada.
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        h.insert(name, header::HeaderValue::from_static(value));
    }
    res
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
    // xterm.js 5.5.0 y su addon de ajuste 0.10.0, tal cual del paquete de npm
    // (licencia MIT en `web/xterm.LICENSE`): la terminal en vivo.
    (
        "xterm.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/xterm.js"),
    ),
    (
        "xterm-addon-fit.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../web/xterm-addon-fit.js"),
    ),
    (
        "xterm.css",
        "text/css; charset=utf-8",
        include_bytes!("../web/xterm.css"),
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
    let h = req.headers();
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // Un WebSocket del navegador no puede mandar `Authorization`: el token va
    // como uno de sus subprotocolos, `token.<token>`.
    let ws = || {
        h.get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok())?
            .split(',')
            .find_map(|p| p.trim().strip_prefix("token."))
    };
    let ok = bearer
        .or_else(ws)
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

/// Como `Query`, con el mismo `{"error": ...}` que `Body` si no se entiende
/// (`?cols=-1`).
struct Params<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Params(v)),
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
/// de `ops` es casi siempre del pedido (agente que no existe, texto vacío…);
/// uno de disco o un `state.json` ilegible es nuestro.
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
            } else if internal(&e) {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::BAD_REQUEST
            };
            error(code, msg)
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Un fallo de disco o un JSON nuestro que no se entiende. Un `cwd` que no
/// existe, no es un directorio o no se puede abrir es del pedido.
fn internal(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| !matches!(io.kind(), NotFound | NotADirectory | PermissionDenied))
            || c.is::<serde_json::Error>()
    })
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
    /// El repositorio en el que trabaja (o su directorio), para agruparlo.
    project: PathBuf,
    project_name: String,
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
                    project: a.project().to_path_buf(),
                    project_name: a.project_name(),
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
    /// En su propio worktree de git.
    #[serde(default)]
    worktree: bool,
}

async fn new_agent(Body(req): Body<NewAgent>) -> Response {
    if let Some(r) = not_absolute(&req.cwd) {
        return r;
    }
    let place = if req.worktree {
        Place::NewWorktree(None)
    } else {
        Place::Dir
    };
    blocking(move || {
        ops::new_agent(
            req.kind,
            Some(req.cwd),
            req.name,
            req.prompt.as_deref(),
            place,
        )
    })
    .await
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
async fn screen(Path(id): Path<String>, Params(size): Params<Size>) -> Response {
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
    Params(since): Params<Since>,
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

async fn restore_agent(Path(id): Path<String>) -> Response {
    blocking(move || ops::restore_agent(&id)).await
}

#[derive(Deserialize)]
struct TermSize {
    cols: u16,
    rows: u16,
}

/// La terminal del agente en vivo: un `tmux attach` a su sesión en un
/// pseudoterminal, por WebSocket. Lo que llega en binario es lo que se
/// escribe; en texto, `{"cols":…,"rows":…}` cuando cambia el tamaño. Lo que
/// sale, en binario, es lo que dibuja tmux.
async fn attach(
    Shared(app): Shared<Arc<App>>,
    Path(id): Path<String>,
    Params(size): Params<TermSize>,
    ws: WebSocketUpgrade,
) -> Response {
    let session = match tokio::task::spawn_blocking(move || ops::attach_session(&id)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return error(StatusCode::NOT_FOUND, format!("{e:#}")),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let closing = app.closing.subscribe();
    ws.protocols(["pastor"])
        .on_upgrade(move |socket| bridge(socket, session, size, closing))
}

async fn bridge(
    mut socket: WebSocket,
    session: String,
    size: TermSize,
    mut closing: tokio::sync::watch::Receiver<bool>,
) {
    let opened = Pty::spawn(tmux::attach_command(&session), size.cols, size.rows)
        .and_then(|pty| Ok((pty.reader()?, pty)));
    let (mut reader, pty) = match opened {
        Ok(p) => p,
        Err(e) => {
            let frame = CloseFrame {
                code: 1011,
                reason: format!("{e:#}").into(),
            };
            let _ = socket.send(ws::Message::Close(Some(frame))).await;
            return;
        }
    };
    // Leer del pseudoterminal bloquea: va en su hilo. Termina cuando sale
    // tmux (al cerrar la terminal o si se acaba la sesión).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    // Si la cerró el otro lado, su cierre ya tiene respuesta en camino.
    let mut answered = false;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(bytes) => {
                    if socket.send(ws::Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(ws::Message::Binary(data))) => {
                    // Escribir puede bloquear si tmux no lee (un pegado largo):
                    // que no se lleve un hilo de tokio sin avisar.
                    if tokio::task::block_in_place(|| pty.write(&data)).is_err() {
                        break;
                    }
                }
                Some(Ok(ws::Message::Text(text))) => {
                    if let Ok(s) = serde_json::from_str::<TermSize>(&text) {
                        let _ = pty.resize(s.cols, s.rows);
                    }
                }
                Some(Ok(ws::Message::Close(_))) => {
                    answered = true;
                    break;
                }
                Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = closing.changed() => break,
        }
    }
    if !answered {
        let _ = socket.send(ws::Message::Close(None)).await;
    }
    // Leer hasta el final es lo que manda la respuesta al cierre y espera la
    // del otro lado; sin eso el navegador lo ve como una conexión cortada.
    let drain = async { while let Some(Ok(_)) = socket.recv().await {} };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), drain).await;
    // Cerrar tmux espera a que salga.
    let _ = tokio::task::spawn_blocking(move || drop(pty)).await;
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
    /// Solo para un agente nuevo: en su propio worktree de git.
    #[serde(default)]
    worktree: bool,
}

async fn assign_task(Path(id): Path<String>, Body(req): Body<Assign>) -> Response {
    let target = match req {
        Assign {
            agent: Some(a),
            kind: None,
            cwd: None,
            worktree: false,
        } => AssignTarget::Existing(a),
        Assign {
            agent: None,
            kind: Some(kind),
            cwd: Some(cwd),
            worktree,
        } => {
            if let Some(r) = not_absolute(&cwd) {
                return r;
            }
            AssignTarget::New {
                kind,
                cwd: Some(cwd),
                worktree,
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

/// Integra la rama de la tarea y la da por hecha. Responde con el resumen.
async fn approve_task(Path(id): Path<String>) -> Response {
    blocking(move || ops::approve_task(&id).map(|message| json!({ "message": message }))).await
}

async fn reject_task(Path(id): Path<String>, Body(req): Body<Message>) -> Response {
    blocking(move || ops::reject_task(&id, &req.text)).await
}

async fn delete_task(Path(id): Path<String>) -> Response {
    blocking(move || ops::delete_task(&id).map(|()| ok())).await
}

// ---------- historial ----------

/// Las estadísticas de la pestaña Historial. Las duraciones van en segundos.
async fn stats(Shared(app): Shared<Arc<App>>) -> Response {
    blocking(move || {
        let s = {
            let mut tail = app.history.lock().unwrap_or_else(PoisonError::into_inner);
            tail.refresh();
            tail.stats(chrono::Local::now().date_naive())
        };
        Ok(json!({
            "since": s.since,
            "sessions": s.sessions,
            "tasks_created": s.tasks_created,
            "tasks_done": s.tasks_done,
            "messages": s.messages,
            "turns": s.turns,
            "avg_session_secs": s.avg_session_secs,
            "avg_task_secs": s.avg_task_secs,
            "agent_secs": s.agent_secs,
            "by_kind": Kind::ALL.iter().zip(&s.by_kind).map(|(k, n)| json!({ "kind": k, "sessions": n })).collect::<Vec<_>>(),
            "projects": s.projects.iter().map(|(p, n)| json!({ "path": p, "sessions": n })).collect::<Vec<_>>(),
            "daily": s.daily.iter().map(|(d, n)| json!({ "day": d, "events": n })).collect::<Vec<_>>(),
        }))
    })
    .await
}

// ---------- límites de uso ----------

/// Lo que queda de cada ventana de límite de las suscripciones.
async fn limits(Shared(app): Shared<Arc<App>>) -> Response {
    blocking(move || {
        let lock = || app.limits.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cache = lock();
        // La primera vez no hay nada que servir: se lee con el candado y las
        // demás consultas esperan este dato en lugar de pedirlo también.
        if cache.usage().is_empty() {
            cache.refresh();
            return Ok(json!(cache.usage()));
        }
        let claim = cache.claim();
        drop(cache);
        // Después, sin el candado mientras se consulta la red: las demás
        // consultas no esperan y sirven el dato de antes.
        if let Some(at) = claim {
            match std::panic::catch_unwind(limits::fetch) {
                Ok(usage) => lock().set(at, usage),
                Err(p) => {
                    lock().forget(at);
                    std::panic::resume_unwind(p);
                }
            }
        }
        Ok(json!(lock().usage()))
    })
    .await
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
            history: Mutex::default(),
            limits: Mutex::default(),
            closing: tokio::sync::watch::Sender::new(false),
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        rt.spawn(async move { axum::serve(listener, router(app)).await });
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        // Sin dos puntos, la cabecera es `Authorization`.
        let auth = auth.map_or(String::new(), |a| match a.split_once(": ") {
            Some((name, value)) => format!("{name}: {value}\r\n"),
            None => format!("Authorization: {a}\r\n"),
        });
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
        assert!(get("/xterm.js", None).contains("text/javascript"));
        assert!(get("/xterm.css", None).contains("text/css"));
        assert!(get("/manifest.webmanifest", None).starts_with("HTTP/1.1 200"));
        assert!(get("/nada.js", None).starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn cabeceras_de_seguridad() {
        for r in [get("/", None), get("/api/tasks", None)] {
            let r = r.to_ascii_lowercase();
            assert!(
                r.contains("content-security-policy: default-src 'self'"),
                "{r}"
            );
            assert!(r.contains("x-content-type-options: nosniff"), "{r}");
            assert!(r.contains("referrer-policy: no-referrer"), "{r}");
        }
    }

    #[test]
    fn el_token_de_un_websocket_va_como_subprotocolo() {
        let ok = get(
            "/api/nada",
            Some("Sec-WebSocket-Protocol: pastor, token.secreto"),
        );
        assert!(ok.starts_with("HTTP/1.1 404"), "{ok}");
        let malo = get(
            "/api/nada",
            Some("Sec-WebSocket-Protocol: pastor, token.otro"),
        );
        assert!(malo.starts_with("HTTP/1.1 401"), "{malo}");
        // Sin el prefijo no vale: el subprotocolo `pastor` no es el token.
        let sin = get("/api/nada", Some("Sec-WebSocket-Protocol: secreto"));
        assert!(sin.starts_with("HTTP/1.1 401"), "{sin}");
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

    #[test]
    fn una_consulta_mal_formada_responde_con_json() {
        let r = get(
            "/api/agents/a1/screen?cols=-1&rows=x",
            Some("Bearer secreto"),
        );
        assert!(
            r.starts_with("HTTP/1.1 400") && r.contains(r#"{"error":"#),
            "{r}"
        );
    }

    #[test]
    fn distingue_errores_internos_de_los_del_pedido() {
        let io = |kind| anyhow::Error::from(std::io::Error::from(kind));
        assert!(internal(
            &io(std::io::ErrorKind::StorageFull).context("no pude guardar state.json")
        ));
        assert!(!internal(
            &io(std::io::ErrorKind::PermissionDenied).context("el directorio /root/x no existe")
        ));
        assert!(!internal(
            &io(std::io::ErrorKind::NotFound).context("el directorio /x no existe")
        ));
        let bad_json = serde_json::from_str::<Value>("{").unwrap_err();
        assert!(internal(
            &anyhow::Error::from(bad_json).context("state.json está corrupto")
        ));
        assert!(!internal(&anyhow::anyhow!("el mensaje está vacío")));
    }
}
