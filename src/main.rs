mod agent;
mod avisos;
mod history;
mod hooks;
mod icons;
mod limits;
mod ntfy;
mod ops;
#[cfg(feature = "serve")]
mod server;
mod store;
mod text;
mod tmux;
#[cfg(feature = "serve")]
mod transcript;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Arg, ArgAction, Command, CommandFactory, FromArgMatches, Parser, Subcommand};

use agent::{Kind, Status};
use ops::AssignTarget;
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
    },
    /// Lista los agentes y su estado
    #[command(alias = "ls")]
    List,
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
    /// Termina un agente
    Kill {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
    },
    /// Gestiona tareas
    #[command(subcommand)]
    Task(TaskCmd),
    /// Sirve una API HTTP para manejar los agentes desde otro dispositivo
    #[cfg(feature = "serve")]
    Serve {
        /// Dirección y puerto (p. ej. la IP de Tailscale)
        #[arg(long, default_value = server::DEFAULT_BIND, value_name = "IP:PUERTO")]
        bind: std::net::SocketAddr,
    },
    /// Uso interno: lo llaman los hooks de Claude Code
    #[command(hide = true)]
    Hook { event: String },
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
    let cli = Cli::from_arg_matches(&cmd.get_matches())
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
        }) => {
            let a = ops::new_agent(kind, cwd, name, prompt.as_deref())?;
            println!(
                "Agente {} ({}) lanzado en {}",
                a.name,
                a.id,
                a.cwd.display()
            );
            Ok(())
        }
        Some(Cmd::List) => {
            hooks::prune_status();
            let state = State::load()?;
            if state.agents.is_empty() {
                println!("No hay agentes. Lanza uno con `pastor new claude`.");
            }
            let snaps = ops::observe(&state, &mut ops::Tracker::default())?;
            for a in &state.agents {
                let st = snaps.get(&a.id).map_or(Status::Dead, |s| s.status);
                println!(
                    "{:<5} {} {:<11} {} {:<9} {}",
                    a.id,
                    st.icon(),
                    st.label(),
                    text::pad_right(&a.name, 20),
                    a.kind,
                    a.cwd.display()
                );
            }
            Ok(())
        }
        Some(Cmd::Attach { agent }) => ops::attach(&agent),
        #[cfg(feature = "serve")]
        Some(Cmd::Serve { bind }) => server::run(bind),
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
        Some(Cmd::Screen { agent }) => {
            println!("{}", ops::screen(&agent)?);
            Ok(())
        }
        Some(Cmd::Kill { agent }) => ops::kill_agent(&agent),
        Some(Cmd::Task(cmd)) => task_cmd(cmd),
    }
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
            for t in &state.tasks {
                let agent = t
                    .agent
                    .as_ref()
                    .and_then(|id| state.agents.iter().find(|a| &a.id == id))
                    .map_or("-", |a| a.name.as_str());
                println!(
                    "{:<5} {:<12} {} {}",
                    t.id,
                    t.status.label(),
                    text::pad_right(agent, 16),
                    t.title
                );
            }
        }
        TaskCmd::Assign {
            task,
            agent,
            new,
            cwd,
        } => {
            let target = match (agent, new) {
                (Some(a), _) => AssignTarget::Existing(a),
                (None, Some(kind)) => AssignTarget::New { kind, cwd },
                (None, None) => unreachable!("clap exige --agent o --new"),
            };
            let a = ops::assign_task(&task, target)?;
            println!("Tarea {task} asignada a {} ({})", a.name, a.id);
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
