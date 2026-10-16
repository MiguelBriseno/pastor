mod agent;
mod hooks;
mod ops;
mod store;
mod tmux;
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
    /// Termina un agente
    Kill {
        /// Id o nombre del agente
        #[arg(value_name = "AGENTE")]
        agent: String,
    },
    /// Gestiona tareas
    #[command(subcommand)]
    Task(TaskCmd),
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
        #[arg(long, value_name = "DIR")]
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
    match cli.cmd {
        None => tui::run(),
        Some(Cmd::Hook { event }) => {
            let _ = hooks::handle(&event);
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
            let state = State::load()?;
            if state.agents.is_empty() {
                println!("No hay agentes. Lanza uno con `pastor new claude`.");
            }
            let snaps = ops::observe(&state, &mut ops::Tracker::default());
            for a in &state.agents {
                let st = snaps.get(&a.id).map_or(Status::Dead, |s| s.status);
                println!(
                    "{:<5} {} {:<11} {:<20} {:<9} {}",
                    a.id,
                    st.icon(),
                    st.label(),
                    a.name,
                    a.kind,
                    a.cwd.display()
                );
            }
            Ok(())
        }
        Some(Cmd::Attach { agent }) => tmux::attach(&State::load()?.find_agent(&agent)?.id),
        Some(Cmd::Send { agent, text }) => ops::send(&agent, &text.join(" ")),
        Some(Cmd::Kill { agent }) => ops::kill_agent(&agent),
        Some(Cmd::Task(cmd)) => task_cmd(cmd),
    }
}

fn task_cmd(cmd: TaskCmd) -> Result<()> {
    match cmd {
        TaskCmd::Add { title, description } => {
            let t = State::update(|s| Ok(s.add_task(title, description.unwrap_or_default())))?;
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
                    "{:<5} {:<12} {:<16} {}",
                    t.id,
                    t.status.label(),
                    agent,
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
                (None, Some(kind)) => AssignTarget::New {
                    kind,
                    cwd: cwd.unwrap_or(std::env::current_dir()?),
                },
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
