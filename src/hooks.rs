//! Hooks de Claude Code: `pastor hook <evento>` escribe el estado del agente
//! en ~/.local/share/pastor/status/<id>.json.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent;
use crate::history::{self, Event};
use crate::ops;
use crate::store::{self, data_dir, ensure_dirs, write_atomic, write_atomic_mode, write_volatile};
use crate::tmux::{self, shell_quote};

#[derive(Serialize, Deserialize, Debug)]
pub struct HookStatus {
    pub state: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub message: String,
    /// Huella de la pantalla del agente cuando se asentó tras el hook. Si la
    /// pantalla ya es otra, el hook es viejo aunque nadie la viera cambiar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<Screen>,
    /// La transcripción de la sesión de Claude Code (llega en cada hook):
    /// de ahí sale la conversación entera para la app del celular.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Shells de fondo que lanzaron los subagentes. Un subagente que espera
    /// el suyo deja de contar como trabajo de fondo y solo queda su shell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_shells: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub hash: u64,
    /// Si el panel cambió de tamaño, el agente se redibuja: no se compara.
    pub size: String,
}

/// FNV-1a: estable entre versiones de pastor, a diferencia de `DefaultHasher`
/// (el hook y quien lo lee pueden ser binarios distintos).
pub fn screen_hash(screen: &str) -> u64 {
    fnv1a(screen.as_bytes())
}

/// FNV-1a de `bytes`.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// Líneas del final que no cuentan para la huella: la caja del prompt y el
/// pie, donde Claude Code pone y quita pistas solo (`/effort`, el modo…).
const FOOTER: usize = 5;

/// Viñeta de una herramienta pendiente, que parpadea mientras espera
/// permiso: `●`, o `⏺` en macOS.
const BLINK: [char; 2] = ['●', '⏺'];

/// `screen` sin el parpadeo de la viñeta, que no es actividad del agente.
fn normalize_blink(screen: &str) -> String {
    screen.replace(BLINK, " ")
}

/// Hash de la pantalla entera para observar agentes que no son Claude Code,
/// cuyo pie no sigue el formato de `FOOTER`.
pub fn activity_hash(screen: &str) -> u64 {
    screen_hash(&normalize_blink(screen))
}

/// Huella de la pantalla para saber si un hook sigue valiendo y si Claude
/// Code hizo algo. Responder un diálogo o interrumpir el turno cambia lo que
/// hay encima del pie.
pub fn fingerprint(screen: &str) -> u64 {
    let screen = normalize_blink(screen);
    let lines: Vec<&str> = screen.trim_end().lines().map(str::trim_end).collect();
    screen_hash(&lines[..lines.len().saturating_sub(FOOTER)].join("\n"))
}

fn status_path(id: &str) -> PathBuf {
    data_dir().join("status").join(format!("{id}.json"))
}

/// El hook y `settle` escriben el mismo estado: que no se pisen.
fn lock_status(id: &str) -> Result<fs::File> {
    store::lock(&data_dir().join("status").join(format!("{id}.lock")))
}

pub fn read_status(id: &str) -> Option<HookStatus> {
    serde_json::from_str(&fs::read_to_string(status_path(id)).ok()?).ok()
}

/// El `.lock` se queda: otro proceso puede estar esperándolo, y si se
/// borrara, el siguiente bloquearía otro archivo. Solo lo quita `prune`, con
/// el agente ya fuera del estado.
pub fn clear_status(id: &str) {
    let _ = fs::remove_file(status_path(id));
}

/// Borra los estados de hook de agentes que ya no existen. Con el candado de
/// `state.json`: un agente entra ahí antes de lanzarse, así que uno recién
/// creado no pierde su estado.
pub fn prune_status() {
    let _ = store::State::with_lock(|s| {
        prune_status_in(&data_dir().join("status"), s);
        // Los de `state.json`: con su candado nadie lo está escribiendo. Los
        // de otros archivos (`claude-settings.json`) con margen.
        remove_stale_tmp(&data_dir(), "state.json.", Duration::ZERO);
        remove_stale_tmp(&data_dir(), "", Duration::from_secs(60));
    });
}

/// Borra los temporales que empiezan por `prefix` de escrituras que no
/// terminaron (el proceso murió entre crearlo y renombrarlo) con más de `age`.
fn remove_stale_tmp(dir: &Path, prefix: &str, age: Duration) {
    let Ok(dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in dir.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|e| e >= age));
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(".tmp") && stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn prune_status_in(dir: &Path, state: &store::State) {
    // Un temporal reciente puede ser de una escritura en curso de su hook.
    remove_stale_tmp(dir, "", Duration::from_secs(60));
    let Ok(dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in dir.flatten() {
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

/// Si este proceso desciende de un panel del agente. Un servicio que arrancó
/// desde él y se desligó (p. ej. un worker que lanza sus propios `claude`)
/// hereda `PASTOR_AGENT` pero ya no cuelga de su panel, y su `Stop` mandaría
/// la tarea del agente a revisar. Si no se puede saber, se da por bueno.
fn from_agent(id: &str) -> bool {
    let panes = match tmux::pane_pids(id) {
        Ok(p) if !p.is_empty() => p,
        _ => return true,
    };
    let me = std::process::id();
    // En Linux se sube por /proc solo por la cadena de padres; si no, una
    // foto de `ps`.
    let verdict = if Path::new("/proc/self/stat").exists() {
        descends_from(proc_parent, me, &panes)
    } else {
        let out = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid="])
            .output();
        let Some(out) = out.ok().filter(|o| o.status.success()) else {
            return true;
        };
        let parents = parse_parents(&String::from_utf8_lossy(&out.stdout));
        descends_from(|p| parents.get(&p).copied(), me, &panes)
    };
    verdict != Some(false)
}

/// Padre de `pid` según `/proc/<pid>/stat` (`pid (nombre) estado ppid …`;
/// el nombre puede llevar espacios y paréntesis).
fn proc_parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Lee la salida de `ps -o pid=,ppid=`.
fn parse_parents(out: &str) -> HashMap<u32, u32> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect()
}

/// Si `pid` desciende de alguno de `ancestors`. `None` si no se puede saber:
/// no se ve el propio proceso ni ningún panel (p. ej. otro espacio de PID),
/// la cadena se corta a medias o no se acaba (un ciclo por pids reutilizados).
fn descends_from(
    parent: impl Fn(u32) -> Option<u32>,
    mut pid: u32,
    ancestors: &[u32],
) -> Option<bool> {
    if !ancestors.iter().any(|&a| parent(a).is_some()) {
        return None;
    }
    for _ in 0..64 {
        if ancestors.contains(&pid) {
            return Some(true);
        }
        match parent(pid)? {
            // Llegó arriba del todo sin pasar por un panel.
            0 => return Some(false),
            ppid if ppid == pid => return None,
            ppid => pid = ppid,
        }
    }
    None
}

/// Lo que importa del JSON que Claude Code manda por stdin. El de
/// PostToolUse trae la respuesta entera de la herramienta (a veces varios
/// MB): con un struct, serde salta el resto sin armar objetos.
#[derive(Deserialize, Default)]
struct Payload {
    // `Option` y no `String`: un `null` no debe tirar el resto del aviso.
    message: Option<String>,
    notification_type: Option<String>,
    transcript_path: Option<String>,
    session_id: Option<String>,
    /// Solo viene si el hook lo lanza un subagente, no el hilo principal.
    agent_id: Option<String>,
    /// En `Stop`: el trabajo de fondo que sigue en marcha y que despertará a
    /// Claude al acabar.
    background_tasks: Option<Vec<BackgroundTask>>,
    tool_name: Option<String>,
}

#[derive(Deserialize)]
struct BackgroundTask {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// La respuesta de Bash, aparte: la de otras herramientas puede no ser un
/// objeto y tumbaría el resto del aviso.
#[derive(Deserialize)]
struct BashHook {
    tool_response: BashResponse,
}

#[derive(Deserialize)]
struct BashResponse {
    #[serde(rename = "backgroundTaskId")]
    background_task_id: Option<String>,
}

/// Tipos de trabajo de fondo que cuentan como que el agente sigue en lo suyo.
/// Los shells solo si los lanzó un subagente: uno de Claude puede ser un
/// servidor de desarrollo que no acaba nunca, y el agente nunca terminaría.
const BACKGROUND_WORK: &[&str] = &["subagent", "workflow"];

/// El estado que deja `event`, o `None` si el aviso no pide nada.
/// `background` es trabajar sin que se vea en la pantalla: Claude acabó su
/// turno pero espera a sus subagentes, o es un subagente el que trabaja.
fn hook_state(event: &str, p: &Payload, subagent_shells: &[String]) -> Option<&'static str> {
    let waits = p.background_tasks.iter().flatten().any(|t| {
        t.kind
            .as_deref()
            .is_some_and(|k| BACKGROUND_WORK.contains(&k))
            || t.id.as_ref().is_some_and(|id| subagent_shells.contains(id))
    });
    Some(match event {
        // Solo trae la sesión: no dice nada del estado.
        "session" => return None,
        "notify" => match p.notification_type.as_deref() {
            Some("idle_prompt") => "idle",
            Some("permission_prompt" | "elicitation_dialog") => "blocked",
            // Versiones sin `notification_type`: se mira el mensaje.
            None | Some("")
                if p.message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("waiting for your input") =>
            {
                "idle"
            }
            None | Some("") => "blocked",
            // Otros avisos (p. ej. `auth_success`) no piden nada.
            Some(_) => return None,
        },
        "idle" if waits => "background",
        "idle" => "idle",
        _ if p.agent_id.is_some() => "background",
        _ => "working",
    })
}

/// Lee y descarta stdin: si el hook sale sin leerlo, a Claude Code le puede
/// llegar un EPIPE al escribir uno grande.
fn drain() {
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
}

/// Llamado por Claude Code. Nunca debe fallar ruidosamente.
pub fn handle(event: &str) -> Result<()> {
    let Ok(id) = std::env::var("PASTOR_AGENT") else {
        drain();
        return Ok(());
    };
    // Un proceso que heredó `PASTOR_AGENT` de un agente ya cerrado (p. ej. un
    // servicio que arrancó desde él y lanza sus propios `claude`) no debe
    // revivir su estado. Si no se puede leer el estado, mejor seguir.
    let loaded = store::State::load();
    if loaded
        .as_ref()
        .is_ok_and(|s| !s.agents.iter().any(|a| a.id == id))
    {
        drain();
        return Ok(());
    }
    // `settle` lo lanza un hook que ya pasó el filtro de abajo, y al salir
    // ese hook queda colgando de init.
    if event == "settle" {
        return settle(&id);
    }
    if !from_agent(&id) {
        drain();
        return Ok(());
    }
    // Entero a memoria y luego `from_slice`: más rápido que `from_reader`, y
    // stdin queda leído hasta el final aunque el JSON venga roto.
    let mut input = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut input);
    let p: Payload = serde_json::from_slice(&input).unwrap_or_default();
    // La de un subagente es la de su hilo principal, pero solo este la cambia
    // (con `/clear` o `/resume`). Una sesión sin transcripción (recién
    // abierta, sin mensajes) no se puede retomar: `--resume` fallaría.
    let resumable = p
        .transcript_path
        .as_deref()
        .is_some_and(|t| Path::new(t).exists());
    if let (Ok(state), Some(session), None, true) = (&loaded, &p.session_id, &p.agent_id, resumable)
    {
        if let Err(e) = remember_session(state, &id, session) {
            log_error(event, &e);
        }
    }
    // Si el aviso no pide nada, ni candado ni lectura. Que no pida nada no
    // depende de los shells de los subagentes.
    if hook_state(event, &p, &[]).is_none() {
        return Ok(());
    }
    ensure_dirs()?;
    let state = {
        let _lock = lock_status(&id)?;
        let prev = read_status(&id);
        let mut subagent_shells = prev
            .as_ref()
            .map(|s| s.subagent_shells.clone())
            .unwrap_or_default();
        let Some(state) = hook_state(event, &p, &subagent_shells) else {
            return Ok(());
        };
        // Mientras espera a sus subagentes, el prompt está quieto y Claude
        // Code puede avisar de que te espera; no es así.
        if event == "notify"
            && state == "idle"
            && prev.as_ref().is_some_and(|s| s.state == "background")
        {
            return Ok(());
        }
        if p.agent_id.is_some() && p.tool_name.as_deref() == Some("Bash") {
            let shell = serde_json::from_slice::<BashHook>(&input)
                .ok()
                .and_then(|b| b.tool_response.background_task_id);
            subagent_shells.extend(shell);
        }
        // Al acabar el turno, solo siguen los que aún corren.
        if let Some(tasks) = &p.background_tasks {
            subagent_shells.retain(|s| tasks.iter().any(|t| t.id.as_ref() == Some(s)));
        }
        // Un subagente que sigue con lo suyo no quita el diálogo que el hilo
        // principal tiene abierto: se queda tal cual, con su huella.
        let keep = p.agent_id.is_some() && prev.as_ref().is_some_and(|s| s.state == "blocked");
        let status = match prev {
            Some(prev) if keep => HookStatus {
                subagent_shells,
                ..prev
            },
            _ => HookStatus {
                state: state.into(),
                at: Utc::now(),
                message: p.message.unwrap_or_default(),
                screen: None,
                transcript: p.transcript_path,
                subagent_shells,
            },
        };
        write_volatile(
            &status_path(&id),
            serde_json::to_string(&status)?.as_bytes(),
        )?;
        // Si quitaron el agente mientras, puede que `clear_status` ya pasara:
        // que no quede su estado. Mirar después de escribir cubre los dos órdenes.
        if store::State::load().is_ok_and(|s| !s.agents.iter().any(|a| a.id == id)) {
            clear_status(&id);
            return Ok(());
        }
        if keep {
            return Ok(());
        }
        state
    };
    // El trabajo de fondo cambia la pantalla sin hooks: no hay huella que valga.
    if state != "working" && state != "background" {
        spawn_settle();
    }

    // Claude terminó su turno: su tarea queda lista para revisar.
    if event == "idle" && state == "idle" {
        history::record(Event::Turn { agent: id.clone() });
        ops::finish_turn(&id)?;
    }
    Ok(())
}

/// Guarda en el agente la sesión de Claude Code en la que va, si cambió: con
/// ella `pastor restore` retoma la conversación.
fn remember_session(state: &store::State, id: &str, session: &str) -> Result<()> {
    let known = state.agents.iter().find(|a| a.id == id);
    if session.is_empty() || known.is_none_or(|a| a.session.as_deref() == Some(session)) {
        return Ok(());
    }
    store::State::update(|s| {
        if let Some(a) = s.agents.iter_mut().find(|a| a.id == id) {
            a.session = Some(session.to_string());
        }
        Ok(())
    })
}

/// Lanza `pastor hook settle` sin esperarlo: Claude Code espera al hook, y
/// la pantalla aún no tiene su forma final (el diálogo, el resumen del turno).
fn spawn_settle() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let Ok(exe) = pastor_exe() else { return };
    // Sin heredar stdout: Claude Code esperaría a que se cierre.
    let _ = Command::new(exe)
        .args(["hook", "settle"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

/// Espera a que la pantalla del agente deje de cambiar y guarda su huella en
/// el estado del hook, si mientras tanto no llegó otro. Si el panel ya no
/// existe (el agente salió), no hay nada que guardar ni que anotar.
fn settle(id: &str) -> Result<()> {
    let Ok(pane) = std::env::var("TMUX_PANE") else {
        return Ok(());
    };
    let Some(at) = read_status(id).map(|s| s.at) else {
        return Ok(());
    };
    let Ok(size) = tmux::pane_size(&pane) else {
        return Ok(());
    };
    // Tres capturas iguales seguidas: una pausa breve antes del dibujo final
    // no basta para darla por quieta.
    let (mut last, mut same) = (None, 0);
    let mut hash = None;
    for _ in 0..12 {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let Ok(screen) = tmux::capture(&pane) else {
            return Ok(());
        };
        let now = fingerprint(&screen);
        same = if last == Some(now) { same + 1 } else { 0 };
        last = Some(now);
        if same == 2 {
            hash = Some(now);
            break;
        }
    }
    // Si no se quedó quieta o cambió de tamaño, mejor sin huella que con una
    // que no es.
    let Some(hash) = hash else { return Ok(()) };
    if tmux::pane_size(&pane).ok().as_ref() != Some(&size) {
        return Ok(());
    }
    let _lock = lock_status(id)?;
    match read_status(id) {
        Some(mut s) if s.at == at => {
            s.screen = Some(Screen { hash, size });
            write_volatile(&status_path(id), serde_json::to_string(&s)?.as_bytes())
        }
        _ => Ok(()),
    }
}

/// Anota en `hooks.log` un hook que falló, porque Claude Code no lo muestra.
pub fn log_error(event: &str, e: &anyhow::Error) {
    use std::io::Write;
    let agent = std::env::var("PASTOR_AGENT").unwrap_or_default();
    let line = format!("{} {agent} {event}: {e:#}\n", Utc::now().to_rfc3339());
    let path = data_dir().join("hooks.log");
    let _ = fs::create_dir_all(data_dir());
    // Que no crezca sin límite: pasado 1 MB se empieza de nuevo.
    let full = fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20);
    let _ = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!full)
        .truncate(full)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Ruta de este binario para los hooks. En Linux, si se reinstaló mientras
/// corría (`cargo install` lo reemplaza), `current_exe` añade " (deleted)" a
/// una ruta en la que ya está el binario nuevo.
fn pastor_exe() -> Result<PathBuf> {
    Ok(strip_deleted(std::env::current_exe()?))
}

fn strip_deleted(exe: PathBuf) -> PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => exe,
    }
}

/// Escribe el settings.json que se pasa a `claude --settings`.
/// Se suma a tu configuración normal de Claude Code, no la reemplaza.
pub fn claude_settings() -> Result<PathBuf> {
    let exe = shell_quote(&pastor_exe()?.to_string_lossy());
    let hook = |ev: &str| json!([{ "hooks": [{ "type": "command", "command": format!("{exe} hook {ev}") }] }]);
    let tool_hook = |ev: &str| json!([{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("{exe} hook {ev}") }] }]);
    let settings = json!({
        "hooks": {
            "SessionStart": hook("session"),
            "UserPromptSubmit": hook("working"),
            "PreToolUse": tool_hook("working"),
            "PostToolUse": tool_hook("working"),
            "Notification": hook("notify"),
            "Stop": hook("idle"),
        }
    });
    ensure_dirs()?;
    let path = data_dir().join("claude-settings.json");
    write_atomic(&path, serde_json::to_string_pretty(&settings)?.as_bytes())?;
    Ok(path)
}

/// Código de `sh` que abre el shell del usuario cuando el agente sale. Ese
/// shell tiene al inicio del PATH un `claude` que lo vuelve a abrir con los
/// hooks. El PATH se ajusta después de leer tu configuración (`.bashrc`,
/// `.zshrc`…), que suele anteponer sus propios directorios.
pub fn relaunch_shell() -> Result<String> {
    let dir = data_dir().join("shell");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin)?;
    write_claude_shim(&bin)?;
    shell_files(&dir)
}

/// Escribe en `dir` la configuración de bash y zsh que antepone `dir/bin`
/// al PATH, y devuelve el código de `sh` que abre el shell con ella.
fn shell_files(dir: &Path) -> Result<String> {
    let bin = dir.join("bin");
    let q = |p: &Path| shell_quote(&p.to_string_lossy());
    let prepend = format!("PATH={}:\"$PATH\"\n", q(&bin));
    write_atomic(
        &dir.join("bashrc"),
        format!("[ -f ~/.bashrc ] && . ~/.bashrc\n{prepend}").as_bytes(),
    )?;
    let zdot = dir.join("zsh");
    fs::create_dir_all(&zdot)?;
    // PASTOR_ZDOTDIR trae tu ZDOTDIR (vacío si no tenías). Tu `.zshenv` se
    // lee con él, y si lo cambia (p. ej. a ~/.config/zsh) lo guardamos para
    // el `.zshrc`: zsh debe seguir leyendo el `.zshrc` de aquí, o el PATH no
    // se ajusta.
    let user_zdotdir = "if [ -n \"$PASTOR_ZDOTDIR\" ]; then ZDOTDIR=\"$PASTOR_ZDOTDIR\"; \
                        else unset ZDOTDIR; fi\n";
    write_atomic(
        &zdot.join(".zshenv"),
        format!(
            "PASTOR_ZSH=\"$ZDOTDIR\"\n{user_zdotdir}\
             [ -f \"${{ZDOTDIR:-$HOME}}/.zshenv\" ] && . \"${{ZDOTDIR:-$HOME}}/.zshenv\"\n\
             PASTOR_ZDOTDIR=\"${{ZDOTDIR-}}\"\n\
             ZDOTDIR=\"$PASTOR_ZSH\"\n\
             unset PASTOR_ZSH\n"
        )
        .as_bytes(),
    )?;
    // El /etc/zshrc de macOS pone HISTFILE=${ZDOTDIR:-$HOME}/.zsh_history
    // cuando ZDOTDIR todavía es el de aquí: lo devolvemos a tu historial.
    write_atomic(
        &zdot.join(".zshrc"),
        format!(
            "PASTOR_ZSH=\"$ZDOTDIR\"\n{user_zdotdir}unset PASTOR_ZDOTDIR\n\
             [ \"$HISTFILE\" = \"$PASTOR_ZSH/.zsh_history\" ] && \
             HISTFILE=\"${{ZDOTDIR:-$HOME}}/.zsh_history\"\n\
             unset PASTOR_ZSH\n\
             [ -f \"${{ZDOTDIR:-$HOME}}/.zshrc\" ] && . \"${{ZDOTDIR:-$HOME}}/.zshrc\"\n{prepend}"
        )
        .as_bytes(),
    )?;

    Ok(format!(
        r#"sh_="${{SHELL:-/bin/sh}}"
case "${{sh_##*/}}" in
  bash) exec "$sh_" --rcfile {bashrc} -i ;;
  zsh) PASTOR_ZDOTDIR="${{ZDOTDIR-}}" ZDOTDIR={zdot} exec "$sh_" -i ;;
  fish) exec "$sh_" -i -C {fish} ;;
  *) PATH={bin}:"$PATH" exec "$sh_" -i ;;
esac"#,
        bashrc = q(&dir.join("bashrc")),
        zdot = q(&zdot),
        fish = shell_quote(&format!("set -gx PATH {} $PATH", q(&bin))),
        bin = q(&bin),
    ))
}

/// `claude` que llama al de verdad con `--settings`, para que los hooks
/// sigan funcionando cuando lo vuelves a abrir a mano. Con headroom pasa
/// primero por él. headroom busca `claude` en el PATH y vuelve a dar con
/// este script: `PASTOR_VIA_HEADROOM` evita el bucle, y `ANTHROPIC_BASE_URL`
/// (que define headroom) que tu alias `claude='headroom wrap claude'` lo
/// envuelva dos veces.
fn write_claude_shim(bin: &PathBuf) -> Result<()> {
    let real = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .filter(|d| d != bin)
            .map(|d| d.join("claude"))
            .find(|f| agent::is_executable(f))
    });
    let Some(real) = real else {
        return Ok(());
    };
    let via_headroom = agent::headroom().map_or(String::new(), |h| {
        format!(
            "[ -z \"$PASTOR_VIA_HEADROOM$ANTHROPIC_BASE_URL\" ] && \
             PASTOR_VIA_HEADROOM=1 exec {} wrap claude -- \"$@\"\n",
            shell_quote(&h.to_string_lossy())
        )
    });
    // Un `claude` que lanza el propio agente (p. ej. `claude -p` desde su
    // herramienta Bash) va sin los hooks: su fin de turno no es el del agente.
    let real = shell_quote(&real.to_string_lossy());
    let script = format!(
        "#!/bin/sh\n[ -n \"$CLAUDECODE\" ] && exec {real} \"$@\"\n\
         {via_headroom}exec {real} --settings {} \"$@\"\n",
        shell_quote(&claude_settings()?.to_string_lossy()),
    );
    write_atomic_mode(&bin.join("claude"), script.as_bytes(), Some(0o755))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traduce_eventos_a_estado() {
        let p = |json: &str| serde_json::from_str::<Payload>(json).unwrap();
        let casos = [
            ("working", "{}", Some("working")),
            ("session", r#"{"session_id":"abc"}"#, None),
            (
                "working",
                r#"{"tool_response":{"enorme":[1,2,3]}}"#,
                Some("working"),
            ),
            ("idle", "{}", Some("idle")),
            (
                "idle",
                r#"{"background_tasks":[{"id":"a1","type":"subagent","status":"running"}]}"#,
                Some("background"),
            ),
            (
                "idle",
                r#"{"background_tasks":[{"id":"b1","type":"workflow","status":"running"}]}"#,
                Some("background"),
            ),
            // Un shell de fondo (p. ej. `npm run dev`) puede no acabar nunca.
            (
                "idle",
                r#"{"background_tasks":[{"id":"b2","type":"shell","status":"running"}]}"#,
                Some("idle"),
            ),
            ("idle", r#"{"background_tasks":[]}"#, Some("idle")),
            ("working", r#"{"agent_id":"a1"}"#, Some("background")),
            (
                "notify",
                r#"{"agent_id":"a1","notification_type":"permission_prompt"}"#,
                Some("blocked"),
            ),
            (
                "notify",
                r#"{"notification_type":"idle_prompt"}"#,
                Some("idle"),
            ),
            (
                "notify",
                r#"{"notification_type":"permission_prompt"}"#,
                Some("blocked"),
            ),
            (
                "notify",
                r#"{"notification_type":"elicitation_dialog"}"#,
                Some("blocked"),
            ),
            ("notify", r#"{"notification_type":"auth_success"}"#, None),
            (
                "notify",
                r#"{"message":"Claude is waiting for your input"}"#,
                Some("idle"),
            ),
            (
                "notify",
                r#"{"message":"Claude needs your permission"}"#,
                Some("blocked"),
            ),
            (
                "notify",
                r#"{"notification_type":"","message":"x"}"#,
                Some("blocked"),
            ),
        ];
        for (event, json, esperado) in casos {
            assert_eq!(hook_state(event, &p(json), &[]), esperado, "{event} {json}");
        }
        // El shell de un subagente que espera a que acabe sí cuenta.
        let shell = p(r#"{"background_tasks":[{"id":"b2","type":"shell","status":"running"}]}"#);
        assert_eq!(
            hook_state("idle", &shell, &["b2".into()]),
            Some("background")
        );
        assert_eq!(hook_state("idle", &shell, &["b9".into()]), Some("idle"));
        let bash = br#"{"tool_name":"Bash","tool_response":{"stdout":"","backgroundTaskId":"b2"}}"#;
        let b: BashHook = serde_json::from_slice(bash).unwrap();
        assert_eq!(b.tool_response.background_task_id.as_deref(), Some("b2"));
        let full = p(r#"{"message":"hola","transcript_path":"/t.jsonl","otro":null}"#);
        assert_eq!(full.message.as_deref(), Some("hola"));
        let nulo = p(r#"{"message":null,"transcript_path":"/t.jsonl"}"#);
        assert_eq!(nulo.transcript_path.as_deref(), Some("/t.jsonl"));
        assert_eq!(full.transcript_path.as_deref(), Some("/t.jsonl"));
    }

    #[test]
    fn quita_el_sufijo_de_binario_reemplazado() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(strip_deleted(p("/bin/pastor (deleted)")), p("/bin/pastor"));
        assert_eq!(strip_deleted(p("/bin/pastor")), p("/bin/pastor"));
    }

    #[test]
    fn borra_los_estados_de_agentes_que_ya_no_existen() {
        let dir = TempHome(std::env::temp_dir().join(format!(
            "pastor-prune-{}-{}",
            std::process::id(),
            store::unique_suffix()
        )));
        fs::create_dir_all(&dir.0).unwrap();
        for f in [
            "a1.json",
            "a1.lock",
            "a2.json",
            "a2.lock",
            "a1.json.9-1.tmp",
            "a2.json.9-2.tmp",
        ] {
            fs::write(dir.0.join(f), "{}").unwrap();
        }
        // Uno de una escritura que murió hace rato.
        let viejo = std::time::SystemTime::now() - Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(dir.0.join("a2.json.9-2.tmp"))
            .unwrap()
            .set_modified(viejo)
            .unwrap();
        let mut state = store::State::default();
        state.add_agent(agent::Kind::Claude, PathBuf::from("/tmp"), None);
        prune_status_in(&dir.0, &state);
        let mut quedan: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        quedan.sort();
        assert_eq!(quedan, ["a1.json", "a1.json.9-1.tmp", "a1.lock"]);
    }

    #[test]
    fn solo_cuentan_los_procesos_del_panel() {
        // 100: panel; 101 claude, 102 hook. 200: worker desligado (padre 1).
        let parents = parse_parents("  1 0\n100 1\n101 100\n102 101\n200 1\n201 200\n202 201\n");
        let de = |pid, panes: &[u32]| descends_from(|p| parents.get(&p).copied(), pid, panes);
        assert_eq!(de(102, &[100]), Some(true));
        assert_eq!(de(202, &[100]), Some(false));
        // Si no se ve el propio proceso o ningún panel, no se sabe.
        assert_eq!(de(999, &[100]), None);
        assert_eq!(de(102, &[555]), None);
        // Un ciclo no lo cuelga ni lo descarta.
        let ciclo = parse_parents("5 6\n6 5\n100 1\n");
        assert_eq!(descends_from(|p| ciclo.get(&p).copied(), 5, &[100]), None);
    }

    #[test]
    fn este_proceso_cuelga_de_su_padre_en_proc() {
        if !Path::new("/proc/self/stat").exists() {
            return;
        }
        let me = std::process::id();
        let padre = proc_parent(me).unwrap();
        assert_eq!(descends_from(proc_parent, me, &[padre]), Some(true));
        assert_eq!(descends_from(proc_parent, padre, &[me]), Some(false));
    }

    /// HOME temporal que se borra al terminar, aunque falle la prueba.
    struct TempHome(PathBuf);

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Abre el shell que queda tras salir el agente, con `home` como HOME, y
    /// devuelve las líneas de `cmd` que empiezan con `pastor:` (la
    /// configuración del sistema puede imprimir otras). `None` si ese shell
    /// no está instalado.
    fn relaunch(shell: &str, home: &Path, cmd: &str) -> Option<String> {
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let sh = agent::find_in_path(shell)?;
        let dir = home.join("pastor");
        fs::create_dir_all(dir.join("bin")).unwrap();
        let script = shell_files(&dir).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", &script])
            .env("HOME", home)
            .env("SHELL", sh)
            .env_remove("ZDOTDIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Sin terminal de control: un shell interactivo podría escribir en
        // la tuya o quitársela a cargo.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(cmd.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<_> = out.lines().filter(|l| l.starts_with("pastor:")).collect();
        Some(lines.join("\n"))
    }

    #[test]
    fn el_shell_lee_tu_configuracion_y_antepone_el_claude_de_pastor() {
        let home = TempHome(std::env::temp_dir().join(format!(
            "pastor-shell-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        let home = home.0.as_path();
        fs::create_dir_all(home.join(".config/zsh")).unwrap();
        let bin = home.join("pastor/bin");
        let expected =
            |zdotdir: &str| format!("pastor: rc=si path={} zdotdir={zdotdir}", bin.display());
        let cmd = "echo \"pastor: rc=$DESDE_RC path=${PATH%%:*} zdotdir=${ZDOTDIR-no}\"\n";

        fs::write(home.join(".bashrc"), "DESDE_RC=si\n").unwrap();
        if let Some(out) = relaunch("bash", home, cmd) {
            assert_eq!(out, expected("no"), "bash");
        }

        fs::write(home.join(".zshrc"), "DESDE_RC=si\n").unwrap();
        if let Some(out) = relaunch("zsh", home, cmd) {
            // Sin ZDOTDIR antes, sin ZDOTDIR después.
            assert_eq!(out, expected("no"), "zsh");
            // Un `.zshenv` que mueve ZDOTDIR no debe saltarse nuestro `.zshrc`.
            fs::remove_file(home.join(".zshrc")).unwrap();
            fs::write(
                home.join(".zshenv"),
                "export ZDOTDIR=\"$HOME/.config/zsh\"\n",
            )
            .unwrap();
            fs::write(home.join(".config/zsh/.zshrc"), "DESDE_RC=si\n").unwrap();
            let zdotdir = home.join(".config/zsh");
            let out = relaunch("zsh", home, cmd).unwrap();
            assert_eq!(out, expected(&zdotdir.to_string_lossy()), "zsh con ZDOTDIR");
        }
    }
}
