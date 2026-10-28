mod agent;
mod avisos;
mod buffer;
mod editor;
mod files;
mod history;
mod hooks;
mod icons;
mod limits;
mod ntfy;
mod ops;
#[cfg(feature = "serve")]
mod pty;
mod remote;
mod search;
#[cfg(feature = "serve")]
mod server;
mod store;
mod syntax;
mod text;
mod tmux;
mod transcript;
mod tui;
mod usage;
mod worktree;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Arg, ArgAction, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use agent::{Kind, Status};
use ops::{AssignTarget, Place};
use store::{State, TaskStatus};

/// Gestor de agentes de código (Claude Code, Codex, opencode). Sin argumentos abre la interfaz.
#[derive(Parser)]
#[command(name = "pastor", version, subcommand_value_name = "COMANDO")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Lanza un agente nuevo
    New {
        /// Tipo de agente: claude, codex u opencode
        #[arg(hide_possible_values = true, value_name = "TIPO")]
        kind: Kind,
        /// Directorio de trabajo (por defecto el actual)
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Nombre para identificarlo (por defecto el nombre de la carpeta)
        #[arg(long, value_name = "NOMBRE")]
        name: Option<String>,
        /// Primer mensaje para el agente
        #[arg(short, long, value_name = "TEXTO")]
        prompt: Option<String>,
        /// Trabaja en su propio worktree de git, en una rama nueva
        #[arg(short, long)]
        worktree: bool,
        /// Lo lanza en otra máquina (ver `pastor host`). Sin --cwd, en su
        /// directorio personal
        #[arg(long, value_name = "MÁQUINA")]
        host: Option<String>,
    },
    /// Lista los agentes y su estado, también los de otras máquinas
    #[command(alias = "ls")]
    List {
        /// En JSON, solo los de esta máquina (para otro pastor)
        #[arg(long)]
        json: bool,
    },
    /// Entra a la terminal de un agente (sal con Ctrl-b d)
    Attach {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
    },
    /// Envía un mensaje a un agente
    Send {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
        /// Texto del mensaje
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "TEXTO"
        )]
        text: Vec<String>,
    },
    /// Pulsa teclas en un agente, para sus menús (Enter, Escape, Down, C-c, 1…)
    Keys {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
        /// Nombres de tmux: Enter, Escape, Tab, BTab, Up, Down, C-c, una letra o cifra…
        #[arg(required = true, value_name = "TECLA")]
        keys: Vec<String>,
    },
    /// Muestra lo que se ve ahora en el panel de un agente
    Screen {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
        /// Antes lo ajusta a este ancho (lo usa otro pastor)
        #[arg(long, hide = true, requires = "rows")]
        cols: Option<u16>,
        #[arg(long, hide = true, requires = "cols")]
        rows: Option<u16>,
    },
    /// Abre (o cierra) una terminal al lado del agente, p. ej. para `npm run dev`
    Term {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
        /// Comando a ejecutar en la terminal (opcional). Un solo argumento se
        /// pasa tal cual al shell; varios se citan uno por uno
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMANDO"
        )]
        cmd: Vec<String>,
        /// Cierra la terminal en vez de abrirla
        #[arg(long, conflicts_with = "cmd")]
        close: bool,
    },
    /// Abre el editor para revisar o cambiar archivos rápido
    Edit {
        /// Archivo, carpeta o agente (id o nombre). Por defecto la carpeta actual
        #[arg(value_name = "RUTA|AGENTE")]
        target: Option<String>,
    },
    /// Uso interno: los archivos de un agente, para el editor de otro pastor
    #[command(hide = true)]
    Files {
        #[arg(value_name = "AGENTE")]
        agent: String,
        /// Imprime este archivo
        #[arg(long, conflicts_with = "write", value_name = "RUTA")]
        read: Option<String>,
        /// Guarda en este archivo lo que llega por la entrada
        #[arg(long, requires_all = ["hash", "bytes"], value_name = "RUTA")]
        write: Option<String>,
        /// El hash del archivo al leerlo: si cambió, no se guarda
        #[arg(long, value_name = "HASH")]
        hash: Option<String>,
        /// Cuántos bytes trae la entrada: si llegan menos (se cortó la
        /// conexión), no se guarda
        #[arg(long, value_name = "N")]
        bytes: Option<usize>,
    },
    /// Termina un agente
    Kill {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
    },
    /// Vuelve a abrir los agentes que se perdieron (p. ej. al reiniciar la
    /// máquina), retomando su conversación
    Restore {
        /// Id o nombre del agente (por defecto, todos los que se perdieron)
        #[arg(value_name = "AGENTE")]
        agent: Option<String>,
    },
    /// Busca en las conversaciones de los agentes de Claude, también de los
    /// que ya terminaron
    Search {
        /// Lo que buscar: los mensajes que tengan todas las palabras
        #[arg(required = true, value_name = "TEXTO")]
        text: Vec<String>,
        /// Solo desde hace tanto (30m, 12h, 7d, 2w) o desde una fecha (AAAA-MM-DD)
        #[arg(long, value_name = "CUÁNDO")]
        desde: Option<String>,
        /// Cuántos mensajes mostrar como máximo
        #[arg(long, default_value_t = search::MAX, value_name = "N")]
        max: usize,
        /// En JSON
        #[arg(long)]
        json: bool,
    },
    /// Gestiona tareas
    #[command(subcommand)]
    Task(TaskCmd),
    /// Registra otras máquinas para manejar sus agentes por SSH
    #[command(subcommand)]
    Host(HostCmd),
    /// Sirve una API HTTP para manejar los agentes desde otro dispositivo
    #[cfg(feature = "serve")]
    Serve {
        /// Dirección y puerto (p. ej. la IP de Tailscale)
        #[arg(long, default_value = server::DEFAULT_BIND, value_name = "IP:PUERTO")]
        bind: std::net::SocketAddr,
        /// Al arrancar, vuelve a abrir los agentes que se perdieron
        #[arg(long)]
        restore: bool,
    },
    /// Uso interno: lo llaman los hooks de Claude Code
    #[command(hide = true)]
    Hook { event: String },
}

#[derive(Subcommand)]
enum HostCmd {
    /// Registra una máquina. Necesita SSH sin contraseña (con llave) y
    /// pastor instalado allá
    Add {
        /// Nombre corto para ella (va delante del id de sus agentes: servidor:a1)
        #[arg(value_name = "NOMBRE")]
        name: String,
        /// Destino de SSH: usuario@host o un alias de ~/.ssh/config
        #[arg(value_name = "DESTINO")]
        dest: String,
    },
    /// Lista las máquinas registradas y si responden
    #[command(alias = "ls")]
    List,
    /// Olvida una máquina (sus agentes siguen corriendo allá)
    Rm {
        #[arg(value_name = "NOMBRE")]
        name: String,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Crea una tarea
    Add {
        /// Título de la tarea
        #[arg(value_name = "TÍTULO")]
        title: String,
        /// Descripción con los detalles para el agente
        #[arg(short, long, value_name = "TEXTO")]
        description: Option<String>,
    },
    /// Lista las tareas
    #[command(alias = "ls")]
    List,
    /// Asigna una tarea a un agente existente o a uno nuevo
    Assign {
        /// Id de la tarea (p. ej. t1)
        #[arg(value_name = "TAREA")]
        task: String,
        /// Agente existente (id o nombre)
        #[arg(
            long,
            conflicts_with = "new",
            required_unless_present = "new",
            value_name = "AGENTE"
        )]
        agent: Option<String>,
        /// Lanza un agente nuevo de este tipo: claude, codex u opencode
        #[arg(long, hide_possible_values = true, value_name = "TIPO")]
        new: Option<Kind>,
        /// Directorio del agente nuevo (por defecto el actual)
        #[arg(long, conflicts_with = "agent", value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// El agente nuevo trabaja en su propio worktree de git, en una rama nueva
        #[arg(short, long, conflicts_with = "agent")]
        worktree: bool,
    },
    /// Muestra lo que cambió en el worktree de una tarea
    Diff {
        /// Id de la tarea
        #[arg(value_name = "TAREA")]
        task: String,
        /// Solo el resumen: qué archivos cambiaron y cuántas líneas
        #[arg(long)]
        stat: bool,
    },
    /// Aprueba una tarea: integra su rama en la rama actual del repositorio,
    /// termina a su agente y borra el worktree
    Approve {
        /// Id de la tarea
        #[arg(value_name = "TAREA")]
        task: String,
    },
    /// Devuelve una tarea a su agente con lo que hay que cambiar
    Reject {
        /// Id de la tarea
        #[arg(value_name = "TAREA")]
        task: String,
        /// Qué hay que cambiar
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "TEXTO"
        )]
        text: Vec<String>,
    },
    /// Marca una tarea como hecha
    Done {
        /// Id de la tarea
        #[arg(value_name = "TAREA")]
        task: String,
    },
    /// Borra una tarea
    Rm {
        /// Id de la tarea
        #[arg(value_name = "TAREA")]
        task: String,
    },
}

/// Traduce al español la ayuda que genera clap. `path` es el comando
/// completo (p. ej. "pastor task assign").
fn localize(cmd: Command, path: &str) -> Command {
    let cmd = if cmd.has_subcommands() {
        cmd.subcommand_value_name("COMANDO")
    } else {
        cmd
    };
    let usage = cmd.clone().render_usage().to_string();
    let usage = usage
        .trim_start_matches("Usage: ")
        .trim_start_matches(cmd.get_name())
        .replace("[OPTIONS]", "[OPCIONES]");
    let mut template = format!("{{about-with-newline}}\nUso: {path}{usage}\n");
    if cmd.has_subcommands() {
        template.push_str("\nComandos:\n{subcommands}\n");
    }
    if cmd.get_positionals().next().is_some() {
        template.push_str("\nArgumentos:\n{positionals}\n");
    }
    template.push_str("\nOpciones:\n{options}\n");
    let cmd = cmd
        .help_template(template)
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Muestra esta ayuda"),
        );
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    names.iter().fold(cmd, |c, n| {
        c.mut_subcommand(n, |s| localize(s, &format!("{path} {n}")))
    })
}

fn main() -> Result<()> {
    let cmd = localize(Cli::command(), "pastor")
        .disable_version_flag(true)
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version)
                .help("Muestra la versión"),
        );
    // Otro pastor, por SSH, manda los argumentos en hexadecimal para que el
    // shell de aquí no los toque.
    let mut args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(remote::HEX_ARGS) {
        let decoded = args[2..]
            .iter()
            .map(|a| remote::unhex(a))
            .collect::<Result<Vec<_>>>()?;
        args.truncate(1);
        args.extend(decoded);
    }
    let cli = Cli::from_arg_matches(&cmd.get_matches_from(args))
        .map_err(|e| e.exit())
        .unwrap();
    // Antes de cualquier cambio, para que la línea base cuente solo lo previo.
    let _ = history::ensure_baseline();
    match cli.cmd {
        None => {
            hooks::prune_status();
            tui::run()
        }
        Some(Cmd::Hook { event }) => {
            // Sin ruido para Claude Code, pero que quede rastro.
            if let Err(e) = hooks::handle(&event) {
                hooks::log_error(&event, &e);
            }
            Ok(())
        }
        Some(Cmd::New {
            kind,
            cwd,
            name,
            prompt,
            worktree,
            host: Some(host),
        }) => {
            let mut args = vec!["new".to_string(), kind.to_string()];
            if let Some(c) = cwd {
                args.extend(["--cwd".into(), c.to_string_lossy().into_owned()]);
            }
            if let Some(n) = name {
                args.push(format!("--name={n}"));
            }
            // Con `=`: si empieza por `-`, que allá no se tome como opción.
            args.extend(prompt.map(|p| format!("--prompt={p}")));
            if worktree {
                args.push("--worktree".into());
            }
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            print!("{}", remote::run(&host, &args)?);
            Ok(())
        }
        Some(Cmd::New {
            kind,
            cwd,
            name,
            prompt,
            worktree,
            host: None,
        }) => {
            let place = if worktree {
                Place::NewWorktree(None)
            } else {
                Place::Dir
            };
            let a = ops::new_agent(kind, cwd, name, prompt.as_deref(), place)?;
            println!(
                "Agente {} ({}) lanzado en {}",
                a.name,
                a.id,
                a.cwd.display()
            );
            if let Some(wt) = &a.worktree {
                println!("Trabaja en la rama {}", wt.branch);
            }
            Ok(())
        }
        Some(Cmd::List { json: true }) => {
            hooks::prune_status();
            let state = State::load()?;
            let snaps = ops::observe(&state, &mut ops::Tracker::default())?;
            let listed: Vec<remote::Listed> = state
                .agents
                .into_iter()
                .map(|a| {
                    let st = snaps.get(&a.id).map_or(Status::Dead, |s| s.status);
                    remote::Listed {
                        agent: a,
                        status: st.key().into(),
                        status_label: st.label().into(),
                    }
                })
                .collect();
            println!("{}", serde_json::to_string(&listed)?);
            Ok(())
        }
        Some(Cmd::List { json: false }) => {
            hooks::prune_status();
            let state = State::load()?;
            let snaps = ops::observe(&state, &mut ops::Tracker::default())?;
            let mut agents: Vec<(store::Agent, Status)> = state
                .agents
                .into_iter()
                .map(|a| {
                    let st = snaps.get(&a.id).map_or(Status::Dead, |s| s.status);
                    (a, st)
                })
                .collect();
            let remotes = remote::all_agents(&remote::hosts()?);
            let mut offline = Vec::new();
            for (host, result) in remotes {
                match result {
                    Ok(list) => agents.extend(list),
                    Err(e) => offline.push(format!("{host}: sin conexión ({e:#})")),
                }
            }
            if agents.is_empty() && offline.is_empty() {
                println!("No hay agentes. Lanza uno con `pastor new claude`.");
            }
            let mut sorted: Vec<store::Agent> = agents.iter().map(|(a, _)| a.clone()).collect();
            store::sort_by_project(&mut sorted);
            let status: HashMap<&str, Status> =
                agents.iter().map(|(a, st)| (a.id.as_str(), *st)).collect();
            let groups: Vec<&[store::Agent]> = sorted.chunk_by(|a, b| a.same_project(b)).collect();
            // Los de otras máquinas llevan su nombre delante: más anchos.
            let id_w = sorted.iter().map(|a| a.id.len()).max().unwrap_or(0).max(5);
            for (i, group) in groups.iter().enumerate() {
                // Con un solo proyecto, sin encabezados.
                if groups.len() > 1 {
                    if i > 0 {
                        println!();
                    }
                    let a = &group[0];
                    println!("{} ({})", a.project_name(), a.project().display());
                }
                for a in *group {
                    let st = status[a.id.as_str()];
                    println!(
                        "{:<id_w$} {} {:<11} {} {:<9} {}",
                        a.id,
                        st.icon(),
                        st.label(),
                        text::pad_right(&a.name, 20),
                        a.kind,
                        a.cwd.display()
                    );
                }
            }
            for line in offline {
                eprintln!("{line}");
            }
            Ok(())
        }
        Some(Cmd::Attach { agent }) => ops::attach(&agent),
        #[cfg(feature = "serve")]
        Some(Cmd::Serve { bind, restore }) => {
            if restore {
                // Que un agente que no se pudo abrir no deje sin API a los demás.
                if let Err(e) = restore_lost() {
                    eprintln!("No pude restaurar los agentes: {e:#}");
                }
            }
            server::run(bind)
        }
        Some(Cmd::Term {
            agent, close: true, ..
        }) => ops::close_term(&agent),
        Some(Cmd::Term { agent, cmd, .. }) => {
            let cmd = term_command(&cmd);
            ops::open_term(&agent, cmd.as_deref())?;
            println!("Terminal abierta junto a {agent}. Entra con `pastor attach {agent}`.");
            Ok(())
        }
        Some(Cmd::Send { agent, text }) => ops::send(&agent, &text.join(" ")),
        Some(Cmd::Keys { agent, keys }) => ops::send_keys(&agent, &keys),
        Some(Cmd::Screen { agent, cols, rows }) => {
            if let (Some(cols), Some(rows)) = (cols, rows) {
                // Si cambió, una pausa para que el agente alcance a redibujarse.
                if ops::fit(&agent, cols, rows)? {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
            println!("{}", ops::screen(&agent)?);
            Ok(())
        }
        Some(Cmd::Edit { target }) => edit(target),
        Some(Cmd::Files {
            agent,
            read,
            write,
            hash,
            bytes,
        }) => {
            let source = files::Source::agent(&agent)?;
            let json = match (read, write, hash) {
                (Some(path), ..) => serde_json::to_string(&source.read(&path)?)?,
                (None, Some(path), Some(hash)) => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    if bytes != Some(text.len()) {
                        anyhow::bail!("el texto llegó incompleto; no guardé {path}");
                    }
                    let hash = source.write(&path, &text, &hash)?;
                    serde_json::to_string(&files::Saved { hash })?
                }
                _ => serde_json::to_string(&source.list()?)?,
            };
            println!("{json}");
            Ok(())
        }
        Some(Cmd::Kill { agent }) => ops::kill_agent(&agent),
        Some(Cmd::Restore { agent: Some(agent) }) => {
            print_restored(&ops::restore_agent(&agent)?);
            Ok(())
        }
        Some(Cmd::Restore { agent: None }) => restore_lost(),
        Some(Cmd::Search {
            text,
            desde,
            max,
            json,
        }) => {
            let since = desde.as_deref().map(search::parse_since).transpose()?;
            let hits = search::run(&search::Query::new(&text.join(" "), since, max)?)?;
            if json {
                println!("{}", serde_json::to_string(&hits)?);
                return Ok(());
            }
            if hits.is_empty() {
                println!("No hay mensajes con eso.");
            }
            for h in &hits {
                let when = h.at.map_or_else(
                    || "—".to_string(),
                    |t| {
                        t.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    },
                );
                let gone = if h.alive { "" } else { ", terminado" };
                println!("{when}  {} ({}{gone}) · {}", h.name, h.agent, h.who());
                println!("  {}", h.snippet);
            }
            Ok(())
        }
        Some(Cmd::Task(cmd)) => task_cmd(cmd),
        Some(Cmd::Host(cmd)) => host_cmd(cmd),
    }
}

/// `pastor edit`: una carpeta, un archivo (con su proyecto en la lista) o
/// el directorio de un agente.
fn edit(target: Option<String>) -> Result<()> {
    let here = std::env::current_dir()?;
    let path = target.as_ref().map_or(here.clone(), |t| here.join(t));
    if path.is_dir() {
        let root = path.canonicalize()?;
        let title = tui::tilde(&root);
        return editor::run(files::Source::dir(root), title, None);
    }
    if path.is_file() {
        let full = path.canonicalize()?;
        let dir = full.parent().unwrap_or(&full).to_path_buf();
        // La raíz del repositorio, para ver el resto del proyecto al lado.
        let root = worktree::toplevel(&dir)
            .and_then(|t| t.canonicalize().ok())
            .filter(|t| full.starts_with(t))
            .unwrap_or(dir);
        let rel = full.strip_prefix(&root)?.to_string_lossy().into_owned();
        let title = tui::tilde(&root);
        return editor::run(files::Source::dir(root), title, Some(rel));
    }
    let key = target.unwrap_or_default();
    let source = files::Source::agent(&key)
        .map_err(|_| anyhow::anyhow!("no existe el archivo, la carpeta ni el agente `{key}`"))?;
    editor::run(source, key, None)
}

/// Restaura los agentes que se perdieron y dice cómo le fue con cada uno.
/// Falla si alguno no se pudo abrir.
fn restore_lost() -> Result<()> {
    let results = ops::restore_lost()?;
    if results.is_empty() {
        println!("No hay agentes que restaurar.");
    }
    let mut failed = 0;
    for (a, result) in &results {
        match result {
            Ok(()) => print_restored(a),
            Err(e) => {
                failed += 1;
                eprintln!("No pude restaurar {} ({}): {e:#}", a.name, a.id);
            }
        }
    }
    if failed > 0 {
        anyhow::bail!(
            "{failed} de {} agentes no se pudieron restaurar",
            results.len()
        );
    }
    Ok(())
}

fn print_restored(a: &store::Agent) {
    println!(
        "Agente {} ({}) restaurado en {}",
        a.name,
        a.id,
        a.cwd.display()
    );
}

/// El comando de `pastor term` como texto para el shell. Un solo argumento va
/// tal cual, para poder escribir `pastor term web "npm run dev | tee log"`;
/// varios se citan uno por uno, para que `git commit -m "dos palabras"` llegue
/// con el mensaje entero.
fn term_command(args: &[String]) -> Option<String> {
    match args {
        [] => None,
        [one] => Some(one.clone()),
        many => Some(
            many.iter()
                .map(|a| tmux::shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

fn host_cmd(cmd: HostCmd) -> Result<()> {
    match cmd {
        HostCmd::Add { name, dest } => {
            remote::add_host(&name, &dest)?;
            // Que se sepa ya si va a funcionar, no la primera vez que se use.
            match remote::agents(&name) {
                Ok(list) => println!(
                    "Máquina {name} registrada: responde y tiene {} agente(s)",
                    list.len()
                ),
                Err(e) => println!("Máquina {name} registrada, pero no responde: {e:#}"),
            }
        }
        HostCmd::List => {
            let hosts = remote::hosts()?;
            if hosts.is_empty() {
                println!(
                    "No hay máquinas. Registra una con `pastor host add servidor usuario@host`."
                );
            }
            let answers: HashMap<_, _> = remote::all_agents(&hosts).into_iter().collect();
            for (name, dest) in &hosts {
                let st = match &answers[name] {
                    Ok(list) => format!("{} agente(s)", list.len()),
                    Err(e) => format!("sin conexión: {e:#}"),
                };
                println!("{:<12} {:<28} {st}", name, dest);
            }
        }
        HostCmd::Rm { name } => remote::remove_host(&name)?,
    }
    Ok(())
}

fn task_cmd(cmd: TaskCmd) -> Result<()> {
    match cmd {
        TaskCmd::Add { title, description } => {
            let t = ops::add_task(title, description.unwrap_or_default())?;
            println!("Tarea {} creada", t.id);
        }
        TaskCmd::List => {
            let state = State::load()?;
            if state.tasks.is_empty() {
                println!("No hay tareas. Crea una con `pastor task add \"título\"`.");
            }
            let mut tail = history::Tail::default();
            tail.refresh();
            let stats = tail.stats(chrono::Local::now().date_naive());
            let overlaps = ops::overlaps(&state);
            for t in &state.tasks {
                let agent = t
                    .agent
                    .as_ref()
                    .and_then(|id| state.agents.iter().find(|a| &a.id == id))
                    .map_or("-", |a| a.name.as_str());
                let cost = stats
                    .spend_by_task
                    .get(&t.id)
                    .map_or_else(|| "-".to_string(), usage::Spend::cost_label);
                println!(
                    "{:<5} {:<12} {} {:>9}  {}",
                    t.id,
                    t.status.label(),
                    text::pad_right(agent, 16),
                    cost,
                    t.title
                );
                if let Some(list) = overlaps.get(&t.id) {
                    println!("      ⚠ cambia {}", ops::describe_overlaps(&state, list));
                }
            }
        }
        TaskCmd::Assign {
            task,
            agent,
            new,
            cwd,
            worktree,
        } => {
            let target = match (agent, new) {
                (Some(a), _) => AssignTarget::Existing(a),
                (None, Some(kind)) => AssignTarget::New {
                    kind,
                    cwd,
                    worktree,
                },
                (None, None) => unreachable!("clap exige --agent o --new"),
            };
            let a = ops::assign_task(&task, target)?;
            println!("Tarea {task} asignada a {} ({})", a.name, a.id);
            if let Some(wt) = &a.worktree {
                println!("Trabaja en la rama {}, en {}", wt.branch, wt.path.display());
            }
        }
        TaskCmd::Diff { task, stat } => {
            let wt = ops::task_worktree(&task)?;
            worktree::diff(&wt, stat, |mut c| Ok(c.status()?))?;
        }
        TaskCmd::Approve { task } => println!("{}", ops::approve_task(&task)?),
        TaskCmd::Reject { task, text } => {
            let a = ops::reject_task(&task, &text.join(" "))?;
            println!("Tarea {task} devuelta a {}", a.name);
        }
        TaskCmd::Done { task } => ops::set_task_status(&task, TaskStatus::Done)?,
        TaskCmd::Rm { task } => ops::delete_task(&task)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cita_los_argumentos_de_term() {
        assert_eq!(term_command(&[]), None);
        assert_eq!(
            term_command(&args(&["npm run dev | tee log"])).as_deref(),
            Some("npm run dev | tee log")
        );
        assert_eq!(
            term_command(&args(&["git", "commit", "-m", "dos palabras"])).as_deref(),
            Some("git commit -m 'dos palabras'")
        );
        assert_eq!(
            term_command(&args(&["echo", "it's"])).as_deref(),
            Some(r"echo 'it'\''s'")
        );
    }

    #[test]
    fn cwd_solo_con_un_agente_nuevo() {
        let assign = |extra: &[&str]| {
            let args = ["pastor", "task", "assign", "t1"].iter().chain(extra);
            Cli::try_parse_from(args).map(|_| ())
        };
        assert!(assign(&["--agent", "a1", "--cwd", "/tmp"]).is_err());
        assert!(assign(&["--new", "claude", "--cwd", "/tmp"]).is_ok());
    }

    #[test]
    fn textos_que_empiezan_por_guion() {
        let cli = Cli::try_parse_from(["pastor", "new", "codex", "--prompt=-v revisa"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::New { prompt: Some(p), .. }) if p == "-v revisa"));
        // Sin texto, `-p` no se traga la opción siguiente.
        assert!(Cli::try_parse_from(["pastor", "new", "codex", "-p", "--cwd", "/x"]).is_err());
    }
}
