//! Interfaz de terminal: pestañas de Agentes, Tareas, Dashboard e Historial.

use std::collections::HashMap;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{Local, NaiveDate};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph,
    Row, Table, TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::agent::{Kind, Status};
use crate::history::{self, Stats};
use crate::icons::{self, pad};
use crate::limits::{self, Usage};
use crate::ntfy;
use crate::ops::{self, AssignTarget, Snapshot, Tracker};
use crate::store::{Agent, State, Task, TaskStatus};
use crate::text::{self, truncate};

const REFRESH: Duration = Duration::from_secs(1);
const SELECTED: Color = Color::Rgb(45, 45, 60);
const ACCENT: Color = Color::Magenta;
/// Columnas de la etiqueta de un campo (`" {name:>11}: "`).
const LABEL_W: usize = 14;
/// Nombre del hilo que observa los paneles.
const OBSERVER: &str = "observador";
/// Nombre del hilo que ejecuta las acciones.
const ACTIONS: &str = "acciones";
/// Último pánico del observador, para mostrarlo al reiniciarlo.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

fn last_panic() -> String {
    LAST_PANIC
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .unwrap_or_default()
}

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    // El hook de ratatui restaura la terminal ante cualquier pánico, también
    // uno del observador o de las acciones, y la TUI seguiría dibujando sobre
    // una terminal normal. Solo la restaura el hilo de la TUI: los demás se
    // recuperan solos o no la necesitan, y su pánico no la toca.
    let restore = std::panic::take_hook();
    let tui = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        let current = std::thread::current();
        match current.name() {
            _ if current.id() == tui => {
                let _ = execute!(io::stdout(), DisableBracketedPaste);
                restore(info);
            }
            Some(OBSERVER) => {
                *LAST_PANIC.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(info.to_string().replace('\n', " "));
            }
            // El de las acciones se atrapa y se vuelve un error de la acción.
            _ => {}
        }
    }));
    // Con bracketed paste, un texto pegado llega entero en vez de tecla por
    // tecla, y sus saltos de línea no confirman el formulario.
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let result = App::new().run(&mut terminal);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Agents,
    Tasks,
    Dashboard,
    History,
}

enum Mode {
    Normal,
    NewAgent(NewAgentForm),
    Send {
        agent: String,
        input: String,
    },
    Term {
        agent: String,
        input: String,
    },
    NewTask {
        title: String,
        desc: String,
        field: usize,
    },
    Assign(AssignForm),
    Confirm {
        msg: String,
        action: Pending,
    },
}

enum Pending {
    KillAgent(String),
    DeleteTask(String),
}

struct NewAgentForm {
    kind: usize,
    cwd: String,
    name: String,
    prompt: String,
    field: usize,
}

enum AssignOpt {
    Agent(String, String),
    New(Kind),
}

struct AssignForm {
    task: String,
    options: Vec<AssignOpt>,
    sel: usize,
    cwd: String,
}

impl AssignForm {
    /// El directorio solo cuenta (y solo se edita) para un agente nuevo.
    fn new_selected(&self) -> bool {
        matches!(self.options[self.sel], AssignOpt::New(_))
    }
}

struct App {
    tab: Tab,
    mode: Mode,
    state: State,
    snaps: HashMap<String, Snapshot>,
    observer: Observer,
    /// Pánicos seguidos del observador, cuándo toca reiniciarlo y desde
    /// cuándo corre el de ahora.
    observer_fails: u32,
    observer_retry: Option<Instant>,
    observer_since: Instant,
    actions: Actions,
    agent_sel: ListState,
    task_sel: TableState,
    /// Tamaños ajustables de los paneles.
    list_width: u16,
    term_pct: u16,
    desc_height: u16,
    flash: Option<(String, Instant)>,
    stats: Stats,
    limits: Vec<Usage>,
    /// Tamaño al que debe dibujar el agente que se ve (id, columnas, filas),
    /// y el último que se le pidió: solo se le pide otra vez si cambia, o
    /// si vuelves a usar la TUI (el celular pudo haberlo cambiado).
    want_fit: Option<(String, u16, u16)>,
    fitted: Option<(String, u16, u16)>,
    /// Desde cuándo `want_fit` tiene su valor actual, y cuándo se pidió el último.
    want_since: Instant,
    fit_sent: Instant,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            tab: Tab::Agents,
            mode: Mode::Normal,
            state: State::default(),
            snaps: HashMap::new(),
            observer: Observer::spawn(HashMap::new(), true),
            observer_fails: 0,
            observer_retry: None,
            observer_since: Instant::now(),
            actions: Actions::spawn(),
            agent_sel: ListState::default(),
            task_sel: TableState::default(),
            list_width: 44,
            term_pct: 40,
            desc_height: 7,
            flash: None,
            stats: Stats::default(),
            limits: Vec::new(),
            want_fit: None,
            fitted: None,
            want_since: Instant::now(),
            fit_sent: Instant::now(),
            quit: false,
        }
    }

    fn run(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        let result = self.event_loop(term);
        // Que una acción a medias (lanzar un agente, por ejemplo) termine,
        // también si el bucle salió por un error de la terminal.
        if self.actions.pending > 0 {
            self.set_flash(
                "Esperando a que termine la acción en curso… (q otra vez: salir sin esperar)"
                    .to_string(),
            );
            let _ = term.draw(|f| self.draw(f));
        }
        self.actions.wait(abandon);
        result
    }

    /// Si el pánico se repite en cada pasada, reiniciarlo enseguida sería
    /// un bucle: cada vez espera el doble, hasta un minuto.
    fn restart_observer(&mut self) {
        match self.observer_retry {
            None => {
                // Si llevaba un rato bien, este pánico no es seguido del anterior.
                if self.observer_since.elapsed() > Duration::from_secs(5 * 60) {
                    self.observer_fails = 0;
                }
                let wait = Duration::from_secs(1 << self.observer_fails.min(6))
                    .min(Duration::from_secs(60));
                self.observer_fails += 1;
                self.observer_retry = Some(Instant::now() + wait);
                self.set_flash(format!(
                    "El observador de paneles falló; se reinicia en {} s. {}",
                    wait.as_secs(),
                    last_panic()
                ));
            }
            Some(at) if Instant::now() >= at => {
                self.observer_retry = None;
                self.observer_since = Instant::now();
                // Con los estados que ya se veían, para no perder un aviso.
                let seen = self.snaps.iter().map(|(id, s)| (id.clone(), s.status));
                self.observer = Observer::spawn(seen.collect(), false);
            }
            Some(_) => {}
        }
    }

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        self.reload();
        while !self.quit {
            if let Some(pass) = self.observer.take() {
                if let Some(snaps) = pass.snaps {
                    self.snaps = snaps;
                }
                if let Some(stats) = pass.stats {
                    self.stats = stats;
                }
                if let Some(limits) = pass.limits {
                    self.limits = limits;
                }
                // Al volver de un agente pueden ser varios: que se vean todos.
                if !pass.flashes.is_empty() {
                    self.set_flash(pass.flashes.join(" · "));
                }
                self.reload();
            }
            while let Ok((result, retry)) = self.actions.results.try_recv() {
                self.actions.pending -= 1;
                self.finish(result, retry);
            }
            if self.observer.died() {
                self.restart_observer();
            }
            let before = self.want_fit.clone();
            term.draw(|f| self.draw(f))?;
            if self.want_fit != before {
                self.want_since = Instant::now();
            }
            self.send_fit();
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        // Si la usas, que el agente vuelva a tu tamaño aunque
                        // el celular lo haya cambiado. Sin repetirlo en cada
                        // tecla: con una flecha sostenida serían decenas.
                        if self.fit_sent.elapsed() > Duration::from_secs(2) {
                            self.fitted = None;
                        }
                        self.on_key(key, term);
                    }
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    // ---------- estado ----------

    /// Termina en la TUI una acción del hilo de acciones. Si falló y no se
    /// abrió otra cosa mientras, vuelve al formulario con lo escrito.
    fn finish(&mut self, result: Result<Done>, retry: Option<Mode>) {
        match result {
            Ok(done) => {
                self.refresh();
                match done {
                    Done::Msg(msg) => self.set_flash(msg),
                    Done::Agent { id, msg } => {
                        self.set_flash(msg);
                        let i = self.state.agents.iter().position(|a| a.id == id);
                        self.agent_sel.select(i);
                    }
                    Done::Quiet => {}
                }
            }
            Err(e) => {
                self.set_flash(format!("Error: {e:#}"));
                if let (Mode::Normal, Some(m)) = (&self.mode, retry) {
                    self.mode = m;
                }
            }
        }
    }

    /// Recarga el estado y pide al observador que mire los paneles ya, para
    /// que lo que se acaba de hacer se vea sin esperar al siguiente segundo.
    fn refresh(&mut self) {
        self.reload();
        self.observer.wake();
    }

    fn reload(&mut self) {
        // La selección sigue al agente y a la tarea, no a la posición: si otro
        // proceso borra uno de arriba, que `T` o ⏎ no caigan sobre otro.
        let agent = self
            .selected_agent()
            .map(|a| (a.id.clone(), a.name.clone()));
        let task = self.selected_task().map(|t| t.id.clone());
        match State::load() {
            Ok(s) => self.state = s,
            Err(e) => self.set_flash(format!("Error: {e:#}")),
        }
        if let Some((id, name)) = agent {
            match self.state.agents.iter().position(|a| a.id == id) {
                Some(i) => self.agent_sel.select(Some(i)),
                // Quitaron el seleccionado: la selección pasa al vecino.
                None => self.set_flash(format!("{name} ya no está; selección movida")),
            }
        }
        if let Some(i) = task.and_then(|id| self.state.tasks.iter().position(|t| t.id == id)) {
            self.task_sel.select(Some(i));
        }

        clamp(&mut self.agent_sel, self.state.agents.len());
        let n = self.state.tasks.len();
        match self.task_sel.selected() {
            _ if n == 0 => self.task_sel.select(None),
            None => self.task_sel.select(Some(0)),
            Some(i) if i >= n => self.task_sel.select(Some(n - 1)),
            _ => {}
        }
    }

    fn status(&self, id: &str) -> Status {
        self.snaps.get(id).map_or(Status::Starting, |s| s.status)
    }

    /// Pide al agente que se ve que dibuje al tamaño de su vista previa. Espera
    /// a que el tamaño se quede quieto (al arrastrar la terminal cambia en
    /// cada cuadro y el agente se redibujaría a cada ancho), y lo hace en otro
    /// hilo: lee el estado y llama a tmux, y el dibujo no debe trabarse.
    fn send_fit(&mut self) {
        if self.want_fit == self.fitted || self.want_since.elapsed() < Duration::from_millis(300) {
            return;
        }
        if let Some((id, cols, rows)) = self.want_fit.clone() {
            // Si falla (el agente ya salió), solo se ve cortado.
            thread::spawn(move || {
                let _ = ops::fit(&id, cols, rows);
            });
            self.fit_sent = Instant::now();
        }
        self.fitted = self.want_fit.clone();
    }

    fn selected_agent(&self) -> Option<&Agent> {
        self.state.agents.get(self.agent_sel.selected()?)
    }

    fn selected_task(&self) -> Option<&Task> {
        self.state.tasks.get(self.task_sel.selected()?)
    }

    fn agent_name(&self, id: &str) -> String {
        self.state
            .agents
            .iter()
            .find(|a| a.id == id)
            .map_or_else(|| format!("{id} (eliminado)"), |a| a.name.clone())
    }

    fn set_flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    // ---------- teclado ----------

    fn on_key(&mut self, key: KeyEvent, term: &mut DefaultTerminal) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        let result = match mode {
            Mode::Normal => self.on_normal(key, term),
            other => self.on_modal(other, key),
        };
        if let Err(e) = result {
            self.set_flash(format!("Error: {e:#}"));
        }
    }

    /// Pega `text` en el campo activo del formulario abierto. Solo los campos
    /// de mensaje y descripción conservan los saltos de línea.
    fn on_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let (field, multiline) = match &mut self.mode {
            Mode::NewAgent(f) => match f.field {
                1 => (&mut f.cwd, false),
                2 => (&mut f.name, false),
                3 => (&mut f.prompt, true),
                _ => return,
            },
            Mode::Send { input, .. } => (input, true),
            Mode::Term { input, .. } => (input, false),
            Mode::NewTask {
                title, desc, field, ..
            } => match field {
                0 => (title, false),
                _ => (desc, true),
            },
            Mode::Assign(f) if f.new_selected() => (&mut f.cwd, false),
            Mode::Assign(_) | Mode::Normal | Mode::Confirm { .. } => return,
        };
        if multiline {
            field.push_str(&text);
        } else {
            field.push_str(text.trim_end_matches('\n').replace('\n', " ").as_str());
        }
    }

    fn on_normal(&mut self, key: KeyEvent, term: &mut DefaultTerminal) -> Result<()> {
        // Los atajos son teclas solas: Ctrl-x o Alt-x no deben terminar un agente.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Ok(());
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('1') => self.tab = Tab::Agents,
            KeyCode::Char('2') => self.tab = Tab::Tasks,
            KeyCode::Char('3') => self.tab = Tab::Dashboard,
            KeyCode::Char('4') => self.tab = Tab::History,
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Agents => Tab::Tasks,
                    Tab::Tasks => Tab::Dashboard,
                    Tab::Dashboard => Tab::History,
                    Tab::History => Tab::Agents,
                }
            }
            _ => match self.tab {
                Tab::Agents => self.on_agents(key, term)?,
                Tab::Tasks => self.on_tasks(key)?,
                Tab::Dashboard | Tab::History => {}
            },
        }
        Ok(())
    }

    fn on_agents(&mut self, key: KeyEvent, term: &mut DefaultTerminal) -> Result<()> {
        let n = self.state.agents.len();
        let selected = self
            .selected_agent()
            .map(|a| (a.id.clone(), a.name.clone()));
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.agent_sel
                    .select(step(self.agent_sel.selected(), n, -1))
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.agent_sel.select(step(self.agent_sel.selected(), n, 1))
            }
            KeyCode::Char('<') => self.list_width = self.list_width.saturating_sub(4).max(20),
            KeyCode::Char('>') => self.list_width = (self.list_width + 4).min(200),
            KeyCode::Char('[') => self.term_pct = (self.term_pct + 5).min(80),
            KeyCode::Char(']') => self.term_pct = self.term_pct.saturating_sub(5).max(20),
            KeyCode::Char('n') => {
                self.mode = Mode::NewAgent(NewAgentForm {
                    kind: 0,
                    cwd: default_cwd(),
                    name: String::new(),
                    prompt: String::new(),
                    field: 1,
                })
            }
            KeyCode::Enter | KeyCode::Char('a') => {
                if let Some((id, _)) = selected {
                    let result = suspend(term, || ops::attach(&id));
                    // Dentro tomó el tamaño de tu terminal: hay que volver a ajustarlo.
                    self.fitted = None;
                    self.refresh();
                    result?;
                }
            }
            KeyCode::Char('s') => {
                if let Some((id, _)) = selected {
                    self.mode = Mode::Send {
                        agent: id,
                        input: String::new(),
                    };
                }
            }
            KeyCode::Char('t') => {
                if let Some(a) = self.selected_agent() {
                    let open = self
                        .snaps
                        .get(&a.id)
                        .and_then(|s| s.term.as_ref())
                        .is_some_and(|t| !t.dead);
                    if open {
                        self.set_flash(format!(
                            "{} ya tiene terminal · ⏎ para verla, T para cerrarla",
                            a.name
                        ));
                    } else {
                        self.mode = Mode::Term {
                            agent: a.id.clone(),
                            input: ops::suggest_term_cmd(a),
                        };
                    }
                }
            }
            KeyCode::Char('T') => {
                if let Some((id, name)) = selected {
                    ops::close_term(&id)?;
                    self.set_flash(format!("Terminal de {name} cerrada"));
                    self.refresh();
                }
            }
            KeyCode::Char('x') => {
                if let Some((id, name)) = selected {
                    self.mode = Mode::Confirm {
                        msg: format!("¿Terminar el agente {name}?"),
                        action: Pending::KillAgent(id),
                    };
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_tasks(&mut self, key: KeyEvent) -> Result<()> {
        let n = self.state.tasks.len();
        let task = self.selected_task().cloned();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.task_sel.select(step(self.task_sel.selected(), n, -1))
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.task_sel.select(step(self.task_sel.selected(), n, 1))
            }
            KeyCode::Char('<') => self.desc_height = (self.desc_height + 2).min(100),
            KeyCode::Char('>') => self.desc_height = self.desc_height.saturating_sub(2).max(3),
            KeyCode::Char('n') => {
                self.mode = Mode::NewTask {
                    title: String::new(),
                    desc: String::new(),
                    field: 0,
                }
            }
            KeyCode::Enter | KeyCode::Char('a') => {
                if let Some(t) = task {
                    let mut options: Vec<AssignOpt> = self
                        .state
                        .agents
                        .iter()
                        .filter(|a| self.status(&a.id) != Status::Dead)
                        .map(|a| AssignOpt::Agent(a.id.clone(), format!("{} ({})", a.name, a.kind)))
                        .collect();
                    options.extend(Kind::ALL.map(AssignOpt::New));
                    self.mode = Mode::Assign(AssignForm {
                        task: t.id,
                        options,
                        sel: 0,
                        cwd: default_cwd(),
                    });
                }
            }
            KeyCode::Char(' ') => {
                if let Some(t) = task {
                    ops::set_task_status(&t.id, t.status.next())?;
                    self.refresh();
                }
            }
            KeyCode::Char('d') => {
                if let Some(t) = task {
                    ops::set_task_status(&t.id, TaskStatus::Done)?;
                    self.refresh();
                }
            }
            KeyCode::Char('x') => {
                if let Some(t) = task {
                    self.mode = Mode::Confirm {
                        msg: format!("¿Borrar la tarea {}?", t.id),
                        action: Pending::DeleteTask(t.id),
                    };
                }
            }
            KeyCode::Char('g') => {
                if let Some(agent) = task.and_then(|t| t.agent) {
                    if let Some(i) = self.state.agents.iter().position(|a| a.id == agent) {
                        self.agent_sel.select(Some(i));
                        self.tab = Tab::Agents;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Maneja una tecla con un formulario abierto. Si el formulario sigue
    /// abierto hay que devolverlo a `self.mode`.
    fn on_modal(&mut self, mode: Mode, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Esc {
            return Ok(());
        }
        match mode {
            Mode::Normal => {}
            Mode::NewAgent(mut f) => match key.code {
                KeyCode::Enter => {
                    let kind = Kind::ALL[f.kind];
                    let name = Some(f.name.clone()).filter(|s| !s.trim().is_empty());
                    let prompt = Some(f.prompt.clone()).filter(|s| !s.trim().is_empty());
                    let Some(cwd) = expand(&f.cwd) else {
                        self.set_flash(NO_CWD.to_string());
                        self.mode = Mode::NewAgent(f);
                        return Ok(());
                    };
                    self.set_flash(format!("Lanzando {kind}…"));
                    self.actions.run(Some(Mode::NewAgent(f)), move || {
                        let a = ops::new_agent(kind, Some(cwd), name, prompt.as_deref())?;
                        Ok(Done::Agent {
                            id: a.id,
                            msg: format!("Agente {} lanzado", a.name),
                        })
                    });
                }
                KeyCode::Tab | KeyCode::Down => {
                    f.field = (f.field + 1) % 4;
                    self.mode = Mode::NewAgent(f);
                }
                KeyCode::BackTab | KeyCode::Up => {
                    f.field = (f.field + 3) % 4;
                    self.mode = Mode::NewAgent(f);
                }
                KeyCode::Left | KeyCode::Right if f.field == 0 => {
                    let d = if key.code == KeyCode::Left {
                        Kind::ALL.len() - 1
                    } else {
                        1
                    };
                    f.kind = (f.kind + d) % Kind::ALL.len();
                    self.mode = Mode::NewAgent(f);
                }
                _ => {
                    match f.field {
                        1 => edit(&mut f.cwd, &key),
                        2 => edit(&mut f.name, &key),
                        3 => edit(&mut f.prompt, &key),
                        _ => {}
                    }
                    self.mode = Mode::NewAgent(f);
                }
            },
            Mode::Send { agent, mut input } => {
                if key.code == KeyCode::Enter {
                    if !input.trim().is_empty() {
                        let name = self.agent_name(&agent);
                        let (a, text) = (agent.clone(), input.clone());
                        self.set_flash(format!("Enviando a {name}…"));
                        // Si falla (el agente salió, por ejemplo), lo escrito vuelve.
                        self.actions
                            .run(Some(Mode::Send { agent, input }), move || {
                                ops::send(&a, &text)?;
                                Ok(Done::Msg(format!("Mensaje enviado a {name}")))
                            });
                    }
                } else {
                    edit(&mut input, &key);
                    self.mode = Mode::Send { agent, input };
                }
            }
            Mode::Term { agent, mut input } => {
                if key.code == KeyCode::Enter {
                    let name = self.agent_name(&agent);
                    let (a, cmd) = (agent.clone(), input.clone());
                    self.actions
                        .run(Some(Mode::Term { agent, input }), move || {
                            ops::open_term(&a, Some(&cmd))?;
                            Ok(Done::Msg(format!(
                                "Terminal abierta junto a {name} · ⏎ para entrar"
                            )))
                        });
                } else {
                    edit(&mut input, &key);
                    self.mode = Mode::Term { agent, input };
                }
            }
            Mode::NewTask {
                mut title,
                mut desc,
                mut field,
            } => match key.code {
                KeyCode::Enter if !title.trim().is_empty() => {
                    let t = match ops::add_task(title.trim().to_string(), desc.trim().to_string()) {
                        Ok(t) => t,
                        Err(e) => {
                            self.mode = Mode::NewTask { title, desc, field };
                            return Err(e);
                        }
                    };
                    self.refresh();
                    let i = self.state.tasks.iter().position(|x| x.id == t.id);
                    self.task_sel.select(i);
                    self.set_flash(format!("Tarea {} creada · ⏎ para asignarla", t.id));
                }
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    field = 1 - field;
                    self.mode = Mode::NewTask { title, desc, field };
                }
                _ => {
                    edit(if field == 0 { &mut title } else { &mut desc }, &key);
                    self.mode = Mode::NewTask { title, desc, field };
                }
            },
            Mode::Assign(mut f) => match key.code {
                KeyCode::Up => {
                    f.sel = f.sel.saturating_sub(1);
                    self.mode = Mode::Assign(f);
                }
                KeyCode::Down => {
                    f.sel = (f.sel + 1).min(f.options.len() - 1);
                    self.mode = Mode::Assign(f);
                }
                KeyCode::Enter => {
                    let target = match &f.options[f.sel] {
                        AssignOpt::Agent(id, _) => AssignTarget::Existing(id.clone()),
                        AssignOpt::New(kind) => match expand(&f.cwd) {
                            Some(cwd) => AssignTarget::New {
                                kind: *kind,
                                cwd: Some(cwd),
                            },
                            None => {
                                self.set_flash(NO_CWD.to_string());
                                self.mode = Mode::Assign(f);
                                return Ok(());
                            }
                        },
                    };
                    let task = f.task.clone();
                    self.set_flash(format!("Asignando {task}…"));
                    self.actions.run(Some(Mode::Assign(f)), move || {
                        let a = ops::assign_task(&task, target)?;
                        Ok(Done::Msg(format!("Tarea {task} asignada a {}", a.name)))
                    });
                }
                _ => {
                    if f.new_selected() {
                        edit(&mut f.cwd, &key);
                    }
                    self.mode = Mode::Assign(f);
                }
            },
            Mode::Confirm { action, .. } => {
                if key.code == KeyCode::Char('y') || key.code == KeyCode::Char('s') {
                    match action {
                        Pending::KillAgent(id) => self.actions.run(None, move || {
                            ops::kill_agent(&id)?;
                            Ok(Done::Quiet)
                        }),
                        Pending::DeleteTask(id) => ops::delete_task(&id)?,
                    }
                    self.refresh();
                }
            }
        }
        Ok(())
    }

    // ---------- dibujo ----------

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.want_fit = None;
        self.draw_header(f, header);
        match self.tab {
            Tab::Agents => self.draw_agents(f, body),
            Tab::Tasks => self.draw_tasks(f, body),
            Tab::Dashboard => self.draw_dashboard(f, body),
            Tab::History => self.draw_history(f, body),
        }
        self.draw_footer(f, footer);
        self.draw_modal(f);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let ic = icons::get();
        let logo_text = format!(" {}pastor ", pad(ic.logo));
        let [logo, tabs, summary] = Layout::horizontal([
            Constraint::Length(text::width(&logo_text) as u16),
            Constraint::Min(0),
            Constraint::Length(30),
        ])
        .areas(area);
        f.render_widget(
            Span::styled(
                logo_text,
                Style::default().fg(Color::Black).bg(ACCENT).bold(),
            ),
            logo,
        );
        let idx = match self.tab {
            Tab::Agents => 0,
            Tab::Tasks => 1,
            Tab::Dashboard => 2,
            Tab::History => 3,
        };
        let t = Tabs::new([
            format!(" 1 {}Agentes ", pad(ic.agents)),
            format!(" 2 {}Tareas ", pad(ic.tasks)),
            format!(" 3 {}Dashboard ", pad(ic.dashboard)),
            format!(" 4 {}Historial ", pad(ic.history)),
        ])
        .select(idx)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::White).bold())
        .divider("│");
        f.render_widget(t, tabs);

        let mut spans = Vec::new();
        for st in [Status::Working, Status::Blocked, Status::Idle] {
            let n = self
                .state
                .agents
                .iter()
                .filter(|a| match self.status(&a.id) {
                    // Como en el Dashboard: el que arranca cuenta como trabajando.
                    Status::Starting => st == Status::Working,
                    s => s == st,
                })
                .count();
            spans.push(Span::styled(
                format!("{} {}  ", ic.status(st), n),
                Style::default().fg(st.color()),
            ));
        }
        f.render_widget(Line::from(spans).alignment(Alignment::Right), summary);
    }

    fn draw_agents(&mut self, f: &mut Frame, area: Rect) {
        let list_w = self.list_width.min(area.width.saturating_sub(20)).max(20);
        let ic = icons::get();
        let kind_w = Kind::ALL
            .iter()
            .map(|k| text::width(&pad(ic.kind(*k))))
            .max()
            .unwrap_or(0);
        let name_w = (list_w as usize).saturating_sub(28 + kind_w).max(6);
        let [left, right] =
            Layout::horizontal([Constraint::Length(list_w), Constraint::Min(0)]).areas(area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {}Agentes ", pad(ic.agents)));

        if self.state.agents.is_empty() {
            let p = Paragraph::new("\nNo hay agentes todavía.\n\nPulsa n para lanzar uno.")
                .alignment(Alignment::Center)
                .block(block);
            f.render_widget(p, left);
        } else {
            let items: Vec<ListItem> = self
                .state
                .agents
                .iter()
                .map(|a| {
                    let st = self.status(&a.id);
                    let term_open = self
                        .snaps
                        .get(&a.id)
                        .and_then(|s| s.term.as_ref())
                        .is_some_and(|t| !t.dead);
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!(" {} ", ic.status(st)),
                            Style::default().fg(st.color()),
                        ),
                        Span::styled(
                            text::pad_right(&truncate(&a.name, name_w), name_w),
                            Style::default().bold(),
                        ),
                        Span::styled(
                            format!(" {}", text::pad_right(&pad(ic.kind(a.kind)), kind_w)),
                            Style::default().fg(kind_color(a.kind)),
                        ),
                        Span::styled(
                            format!("{:<9}", a.kind.to_string()),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{:<11}", st.label()),
                            Style::default().fg(st.color()),
                        ),
                        Span::styled(
                            if term_open { ic.term } else { " " },
                            Style::default().fg(Color::Cyan),
                        ),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::default().bg(SELECTED));
            f.render_stateful_widget(list, left, &mut self.agent_sel);
        }

        let Some(a) = self.selected_agent() else {
            f.render_widget(Block::bordered().border_type(BorderType::Rounded), right);
            return;
        };
        let snap = self.snaps.get(&a.id);
        let screen = snap.map_or("", |s| s.screen.as_str());

        // Con terminal abierta, la vista se divide igual que en tmux: agente | terminal.
        let right = match snap.and_then(|s| s.term.as_ref()) {
            Some(t) => {
                let [l, r] = Layout::horizontal([
                    Constraint::Percentage(100 - self.term_pct),
                    Constraint::Percentage(self.term_pct),
                ])
                .areas(right);
                let title = match (&t.cmd, t.dead) {
                    (_, true) => format!(" {}terminal · cerrada ", pad(ic.term)),
                    (Some(c), false) => format!(" {}terminal · {c} ", pad(ic.term)),
                    (None, false) => format!(" {}terminal ", pad(ic.term)),
                };
                let block = Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(title);
                let h = r.height.saturating_sub(2) as usize;
                f.render_widget(Paragraph::new(tail(&t.screen, h)).block(block), r);
                l
            }
            None => right,
        };
        let h = right.height.saturating_sub(2) as usize;
        let text = tail(screen, h);
        // Que el agente dibuje al tamaño de su vista, sin los bordes: así sus
        // líneas no se cortan.
        let fit = (
            a.id.clone(),
            right.width.saturating_sub(2),
            right.height.saturating_sub(2),
        );

        let st = self.status(&a.id);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(st.color()))
            .title(Line::from(vec![
                Span::styled(
                    format!(" {} ", ic.status(st)),
                    Style::default().fg(st.color()),
                ),
                Span::styled(a.name.clone(), Style::default().bold()),
                Span::raw(" · "),
                Span::styled(
                    format!("{}{}", pad(ic.kind(a.kind)), a.kind),
                    Style::default().fg(kind_color(a.kind)),
                ),
                Span::raw(format!(" · {}{} ", pad(ic.folder), tilde(&a.cwd))),
            ]));
        if let Some(t) = a.task.as_ref().and_then(|t| self.state.find_task(t).ok()) {
            block = block.title_bottom(format!(
                " {}tarea {}: {} ",
                pad(ic.task),
                t.id,
                truncate(&t.title, 50)
            ));
        }
        f.render_widget(Paragraph::new(text).block(block), right);
        self.want_fit = Some(fit);
    }

    fn draw_tasks(&mut self, f: &mut Frame, area: Rect) {
        let ic = icons::get();
        let desc_h = self.desc_height.min(area.height.saturating_sub(5)).max(3);
        let [top, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(desc_h)]).areas(area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {}Tareas ", pad(ic.tasks)));

        if self.state.tasks.is_empty() {
            let p = Paragraph::new("\nNo hay tareas todavía.\n\nPulsa n para crear una.")
                .alignment(Alignment::Center)
                .block(block);
            f.render_widget(p, top);
        } else {
            let rows: Vec<Row> = self
                .state
                .tasks
                .iter()
                .map(|t| {
                    let agent = match &t.agent {
                        Some(id) => {
                            let st = self.status(id);
                            Line::from(vec![
                                Span::styled(
                                    format!("{} ", ic.status(st)),
                                    Style::default().fg(st.color()),
                                ),
                                Span::raw(self.agent_name(id)),
                            ])
                        }
                        None => Line::from("-".dark_gray()),
                    };
                    Row::new(vec![
                        Cell::from(t.id.clone()),
                        Cell::from(Span::styled(
                            format!("{}{}", pad(ic.task_status(t.status)), t.status.label()),
                            Style::default().fg(task_color(t.status)),
                        )),
                        Cell::from(t.title.clone()),
                        Cell::from(agent),
                    ])
                })
                .collect();
            let table = Table::new(
                rows,
                [
                    Constraint::Length(5),
                    Constraint::Length(14),
                    Constraint::Min(10),
                    Constraint::Length(22),
                ],
            )
            .header(
                Row::new(["ID", "Estado", "Título", "Agente"])
                    .style(Style::default().fg(Color::DarkGray).bold()),
            )
            .block(block)
            .row_highlight_style(Style::default().bg(SELECTED));
            f.render_stateful_widget(table, top, &mut self.task_sel);
        }

        let desc = self.selected_task().map_or(String::new(), |t| {
            if t.description.is_empty() {
                "(sin descripción)".to_string()
            } else {
                t.description.clone()
            }
        });
        let p = Paragraph::new(desc).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Descripción "),
        );
        f.render_widget(p, bottom);
    }

    fn draw_dashboard(&self, f: &mut Frame, area: Rect) {
        let ic = icons::get();
        let named = |icon: &str, name: &str| format!("{}{name}", pad(icon));
        let rows = self.limits.iter().map(|u| u.windows.len().max(1)).max();
        let [r1, r2, r4, r3] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Length(rows.map_or(0, |n| n as u16 + 2)),
            Constraint::Min(0),
        ])
        .areas(area);
        self.draw_limits(f, r4);

        let count = |st: Status| {
            self.state
                .agents
                .iter()
                .filter(|a| self.status(&a.id) == st)
                .count()
        };
        let [a, b, c, d, e] = Layout::horizontal([Constraint::Ratio(1, 5); 5]).areas(r1);
        stat(
            f,
            a,
            &named(ic.agents, "agentes"),
            self.state.agents.len().to_string(),
            Color::White,
        );
        stat(
            f,
            b,
            &named(ic.status(Status::Working), "trabajando"),
            (count(Status::Working) + count(Status::Starting)).to_string(),
            Status::Working.color(),
        );
        stat(
            f,
            c,
            &named(ic.status(Status::Blocked), "esperándote"),
            count(Status::Blocked).to_string(),
            Status::Blocked.color(),
        );
        stat(
            f,
            d,
            &named(ic.status(Status::Idle), "inactivos"),
            count(Status::Idle).to_string(),
            Status::Idle.color(),
        );
        stat(
            f,
            e,
            &named(ic.status(Status::Dead), "terminados"),
            count(Status::Dead).to_string(),
            Status::Dead.color(),
        );

        let tcount = |st: TaskStatus| self.state.tasks.iter().filter(|t| t.status == st).count();
        let total = self.state.tasks.len();
        let pct = (tcount(TaskStatus::Done) * 100)
            .checked_div(total)
            .unwrap_or(0);
        let [a, b, c, d, e] = Layout::horizontal([Constraint::Ratio(1, 5); 5]).areas(r2);
        for (area, st) in [
            (a, TaskStatus::Todo),
            (b, TaskStatus::InProgress),
            (c, TaskStatus::Review),
            (d, TaskStatus::Done),
        ] {
            let name = named(ic.task_status(st), st.label());
            stat(f, area, &name, tcount(st).to_string(), task_color(st));
        }
        stat(
            f,
            e,
            &named(ic.done, "completado"),
            format!("{pct}%"),
            ACCENT,
        );

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(r3);
        let mut items: Vec<ListItem> = Vec::new();
        for a in &self.state.agents {
            if self.status(&a.id) == Status::Blocked {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {} ", ic.status(Status::Blocked)),
                        Style::default().fg(Status::Blocked.color()),
                    ),
                    Span::raw(format!("{} está esperando tu respuesta", a.name)),
                ])));
            }
        }
        for t in self
            .state
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Review)
        {
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", ic.task_status(TaskStatus::Review)),
                    Style::default().fg(task_color(TaskStatus::Review)),
                ),
                Span::raw(format!("{} {} — revisar", t.id, t.title)),
            ])));
        }
        if items.is_empty() {
            items.push(ListItem::new(" Nada pendiente ✓".dark_gray()));
        }
        f.render_widget(
            List::new(items).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(format!(" {}Necesita tu atención ", pad(ic.bell))),
            ),
            left,
        );

        let bars: Vec<Bar> = Kind::ALL
            .iter()
            .map(|k| {
                let n = self.state.agents.iter().filter(|a| a.kind == *k).count() as u64;
                Bar::default()
                    .value(n)
                    .label(Line::from(format!("{}{k}", pad(ic.kind(*k)))))
                    .style(Style::default().fg(kind_color(*k)))
                    .value_style(Style::default().fg(Color::Black).bg(kind_color(*k)).bold())
            })
            .collect();
        let chart = BarChart::default()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(format!(" {}Agentes por tipo ", pad(ic.agents))),
            )
            .data(BarGroup::default().bars(&bars))
            .bar_width(10)
            .bar_gap(2);
        f.render_widget(chart, right);
    }

    /// Una caja por suscripción con lo que queda de cada ventana.
    fn draw_limits(&self, f: &mut Frame, area: Rect) {
        let ic = icons::get();
        let n = self.limits.len().max(1) as u32;
        let boxes = Layout::horizontal(vec![Constraint::Ratio(1, n); n as usize]).split(area);
        let now = chrono::Utc::now();
        for (u, &r) in self.limits.iter().zip(boxes.iter()) {
            let color = kind_color(u.kind);
            let width = r.width.saturating_sub(2) as usize;
            let name_w = u
                .windows
                .iter()
                .map(|w| w.name.chars().count())
                .max()
                .unwrap_or(0);
            // Una que ya pasó (el dato es de hace hasta `TTL`) no dice nada.
            let resets: Vec<Option<String>> = u
                .windows
                .iter()
                .map(|w| {
                    w.resets_at
                        .filter(|t| *t > now)
                        .map(|t| history::human((t - now).num_seconds()))
                })
                .collect();
            // De la más completa a la más corta, la primera en la que quepan
            // todas las líneas, para que la caja quede pareja: con dos cajas
            // en 80 columnas, cada una tiene 38.
            const FORMS: [(usize, &str, Option<&str>); 5] = [
                (10, " libre", Some(" · se reinicia en ")),
                (10, " libre", Some(" ↻ ")),
                (10, "", Some(" ↻ ")),
                (5, "", Some(" ↻ ")),
                (5, "", None),
            ];
            let (bar, libre, reset_prefix) = FORMS
                .into_iter()
                .find(|(bar, libre, prefix)| {
                    resets.iter().all(|reset| {
                        let reset = match (prefix, reset) {
                            (Some(p), Some(r)) => p.chars().count() + r.chars().count(),
                            _ => 0,
                        };
                        name_w + 2 + bar + 5 + libre.len() + reset <= width
                    })
                })
                .unwrap_or(FORMS[FORMS.len() - 1]);
            let lines: Vec<Line> = match &u.error {
                Some(e) => vec![Line::from(format!(" {e}").dark_gray())],
                None => u
                    .windows
                    .iter()
                    .zip(&resets)
                    .map(|(w, reset)| {
                        let left = w.left_pct();
                        let reset = match (reset_prefix, reset) {
                            (Some(p), Some(r)) => format!("{p}{r}"),
                            _ => String::new(),
                        };
                        Line::from(vec![
                            Span::raw(format!(" {:<name_w$} ", w.name)),
                            Span::styled(meter(left, bar), Style::default().fg(limit_color(left))),
                            Span::styled(
                                format!(" {left:>3.0}%{libre}"),
                                Style::default().fg(limit_color(left)).bold(),
                            ),
                            Span::raw(reset).dark_gray(),
                        ])
                    })
                    .collect(),
            };
            let head = format!(" {}{} ", pad(ic.kind(u.kind)), u.kind);
            let plan = u.plan.as_ref().map_or(String::new(), |p| format!("({p}) "));
            let age = u.updated_at.filter(|_| u.stale).map_or(String::new(), |t| {
                format!("· hace {} ", history::human((now - t).num_seconds()))
            });
            // Si no cabe todo, la antigüedad importa más que el plan.
            let title = [
                format!("{head}{plan}{age}"),
                format!("{head}{age}"),
                head.clone(),
            ]
            .into_iter()
            .find(|t| t.chars().count() <= width)
            .unwrap_or(head);
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(color))
                        .title(title),
                ),
                r,
            );
        }
    }

    fn draw_history(&self, f: &mut Frame, area: Rect) {
        let ic = icons::get();
        let s = &self.stats;
        let named = |icon: &str, name: &str| format!("{}{name}", pad(icon));
        let [r1, r2, r3] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Min(0),
        ])
        .areas(area);

        let since = s.since.map_or_else(
            || "—".to_string(),
            |d| {
                d.with_timezone(&chrono::Local)
                    .format("%d/%m/%Y")
                    .to_string()
            },
        );
        let days = s.since.map_or(0, |d| (chrono::Utc::now() - d).num_days());
        let [a, b, c, d, e, g] = Layout::horizontal([Constraint::Ratio(1, 6); 6]).areas(r1);
        stat(f, a, &format!("desde (hace {days} días)"), since, ACCENT);
        stat(
            f,
            b,
            &named(ic.agents, "sesiones"),
            s.sessions.to_string(),
            Color::White,
        );
        stat(
            f,
            c,
            &named(ic.tasks, "tareas creadas"),
            s.tasks_created.to_string(),
            Color::White,
        );
        stat(
            f,
            d,
            &named(ic.task_status(TaskStatus::Done), "tareas hechas"),
            s.tasks_done.to_string(),
            Color::Green,
        );
        stat(
            f,
            e,
            "mensajes enviados",
            s.messages.to_string(),
            Color::Cyan,
        );
        stat(
            f,
            g,
            "turnos de Claude",
            s.turns.to_string(),
            kind_color(Kind::Claude),
        );

        let dash = |v: Option<i64>| v.map_or_else(|| "—".to_string(), history::human);
        let per_day = format!("{:.1}", s.sessions as f64 / days.max(1) as f64);
        let [a, b, c, d] = Layout::horizontal([Constraint::Ratio(1, 4); 4]).areas(r2);
        stat(
            f,
            a,
            "duración media de sesión",
            dash(s.avg_session_secs),
            Color::White,
        );
        stat(
            f,
            b,
            "tiempo medio por tarea",
            dash(s.avg_task_secs),
            Color::White,
        );
        stat(
            f,
            c,
            "tiempo total de agentes",
            history::human(s.agent_secs),
            Color::White,
        );
        stat(f, d, "sesiones por día", per_day, Color::White);

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(r3);
        let bars: Vec<Bar> = s
            .daily
            .iter()
            .map(|(day, n)| {
                Bar::default()
                    .value(*n)
                    .label(Line::from(day.format("%d").to_string()))
                    .style(Style::default().fg(ACCENT))
                    .value_style(Style::default().fg(Color::Black).bg(ACCENT).bold())
            })
            .collect();
        // Que las barras llenen el ancho disponible.
        let n = s.daily.len().max(1) as u16;
        let bar_w = (left.width.saturating_sub(2) / n)
            .saturating_sub(1)
            .clamp(2, 8);
        f.render_widget(
            BarChart::default()
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(format!(
                            " {}Actividad de los últimos {} días ",
                            pad(ic.history),
                            history::DAYS
                        ))
                        .title_bottom(Line::from(
                            " sesiones, tareas, mensajes y turnos por día ".dark_gray(),
                        )),
                )
                .data(BarGroup::default().bars(&bars))
                .bar_width(bar_w)
                .bar_gap(1),
            left,
        );

        let [kinds, projects] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);
        let bars: Vec<Bar> = Kind::ALL
            .iter()
            .zip(&s.by_kind)
            .map(|(k, n)| {
                Bar::default()
                    .value(*n)
                    .label(Line::from(format!("{}{k}", pad(ic.kind(*k)))))
                    .style(Style::default().fg(kind_color(*k)))
                    .value_style(Style::default().fg(Color::Black).bg(kind_color(*k)).bold())
            })
            .collect();
        f.render_widget(
            BarChart::default()
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(format!(" {}Sesiones por tipo ", pad(ic.agents))),
                )
                .data(BarGroup::default().bars(&bars))
                .bar_width(10)
                .bar_gap(2),
            kinds,
        );

        let room = projects.width.saturating_sub(10) as usize;
        let mut items: Vec<ListItem> = s
            .projects
            .iter()
            .take(projects.height.saturating_sub(2) as usize)
            .map(|(p, n)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {n:>4}  "), Style::default().fg(ACCENT).bold()),
                    Span::raw(truncate(&tilde(p), room.max(4))),
                ]))
            })
            .collect();
        if items.is_empty() {
            items.push(ListItem::new(
                " Aún no hay sesiones registradas".dark_gray(),
            ));
        }
        f.render_widget(
            List::new(items).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(format!(" {}Proyectos con más sesiones ", pad(ic.folder))),
            ),
            projects,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if let Some((msg, at)) = &self.flash {
            if at.elapsed() < Duration::from_secs(5) {
                f.render_widget(
                    Span::styled(
                        format!(" {}{msg}", pad(icons::get().bell)),
                        Style::default().fg(Color::Yellow),
                    ),
                    area,
                );
                return;
            }
        }
        let help = match (&self.mode, self.tab) {
            (Mode::Normal, Tab::Agents) => "↑↓ mover · ⏎ entrar · n nuevo · s mensaje · t terminal · T cerrar terminal · x terminar · < > ancho lista · [ ] ancho terminal · q salir",
            (Mode::Normal, Tab::Tasks) => "↑↓ mover · n nueva · ⏎ asignar · espacio avanzar estado · d hecha · g ir al agente · x borrar · < > alto descripción",
            (Mode::Normal, Tab::Dashboard | Tab::History) => "1-4 pestañas · Tab siguiente · q salir",
            (Mode::Confirm { .. }, _) => "y confirmar · cualquier otra tecla cancela",
            (Mode::Assign(form), _) if form.new_selected() => {
                "↑↓ elegir · escribe el directorio · ⏎ asignar · Esc cancelar"
            }
            (Mode::Assign(_), _) => "↑↓ elegir · ⏎ asignar · Esc cancelar",
            _ => "Tab cambiar campo · ⏎ confirmar · Esc cancelar",
        };
        f.render_widget(
            Span::styled(format!(" {help}"), Style::default().fg(Color::DarkGray)),
            area,
        );
    }

    fn draw_modal(&self, f: &mut Frame) {
        match &self.mode {
            Mode::Normal => {}
            Mode::NewAgent(form) => {
                let w = modal_width(f, 100);
                let mut kinds = vec![Span::styled(
                    label("Tipo", form.field == 0),
                    label_style(form.field == 0),
                )];
                for (i, k) in Kind::ALL.iter().enumerate() {
                    let style = if i == form.kind {
                        Style::default().fg(Color::Black).bg(kind_color(*k)).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    let icon = pad(icons::get().kind(*k));
                    kinds.push(Span::styled(format!(" {icon}{k} "), style));
                    kinds.push(Span::raw(" "));
                }
                if form.field == 0 {
                    kinds.push(Span::styled("← →", Style::default().fg(Color::DarkGray)));
                }
                let mut lines = vec![Line::from(kinds), Line::raw("")];
                lines.extend(field_lines("Directorio", &form.cwd, form.field == 1, w));
                lines.extend(field_lines("Nombre", &form.name, form.field == 2, w));
                let room = modal_room(f, 2).saturating_sub(lines.len());
                lines.extend(keep_tail(
                    field_lines("Mensaje", &form.prompt, form.field == 3, w),
                    room,
                ));
                let inner = popup(f, " Nuevo agente ", w, lines.len() as u16 + 4);
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::Send { agent, input } => {
                let w = modal_width(f, 100);
                let mut lines = vec![Line::raw("")];
                let room = modal_room(f, 0).saturating_sub(lines.len());
                lines.extend(keep_tail(field_lines("Mensaje", input, true, w), room));
                let inner = popup(
                    f,
                    &format!(" Mensaje para {} ", self.agent_name(agent)),
                    w,
                    lines.len() as u16 + 2,
                );
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::Term { agent, input } => {
                let w = modal_width(f, 100);
                let mut lines = vec![Line::raw("")];
                let room = modal_room(f, 0).saturating_sub(lines.len() + 1);
                lines.extend(keep_tail(field_lines("Comando", input, true, w), room));
                lines.push(Line::from(
                    "  Déjalo vacío para abrir solo una terminal.".dark_gray(),
                ));
                let inner = popup(
                    f,
                    &format!(" Terminal junto a {} ", self.agent_name(agent)),
                    w,
                    lines.len() as u16 + 2,
                );
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::NewTask { title, desc, field } => {
                let w = modal_width(f, 100);
                let mut lines = vec![Line::raw("")];
                lines.extend(field_lines("Título", title, *field == 0, w));
                let room = modal_room(f, 0).saturating_sub(lines.len());
                lines.extend(keep_tail(
                    field_lines("Descripción", desc, *field == 1, w),
                    room,
                ));
                let inner = popup(f, " Nueva tarea ", w, lines.len() as u16 + 2);
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::Assign(form) => {
                let w = modal_width(f, 64);
                let mut lines = vec![Line::from(" ¿A quién se la das?".dark_gray())];
                // El directorio siempre se ve; las opciones, en una ventana
                // que sigue a la elegida si no caben todas.
                let room = modal_room(f, 1);
                let dir = keep_tail(
                    field_lines("Directorio", &form.cwd, form.new_selected(), w),
                    room.saturating_sub(3),
                );
                let shown = room.saturating_sub(2 + dir.len()).max(1);
                let first = form.sel.saturating_sub(shown - 1);
                for (i, opt) in form.options.iter().enumerate().skip(first).take(shown) {
                    let text = match opt {
                        AssignOpt::Agent(_, label) => format!("  {label}"),
                        AssignOpt::New(k) => format!("+ nuevo agente {k}"),
                    };
                    let style = if i == form.sel {
                        Style::default().bg(SELECTED).bold()
                    } else {
                        Style::default()
                    };
                    let marker = if i == form.sel { " ▶ " } else { "   " };
                    lines.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
                }
                lines.push(Line::raw(""));
                lines.extend(dir);
                let inner = popup(
                    f,
                    &format!(" Asignar {} ", form.task),
                    w,
                    lines.len() as u16 + 3,
                );
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::Confirm { msg, .. } => {
                let inner = popup(f, " Confirmar ", 50, 4);
                let lines = vec![Line::raw(""), Line::from(format!(" {msg}  (y/n)").bold())];
                f.render_widget(Paragraph::new(lines), inner);
            }
        }
    }
}

// ---------- utilidades ----------

/// Sale de la TUI mientras corre `f` (p. ej. tmux attach) y vuelve después.
fn suspend<T>(term: &mut DefaultTerminal, f: impl FnOnce() -> Result<T>) -> Result<T> {
    // Cada paso aunque falle el anterior: a medias, la TUI seguiría dibujando
    // fuera de la pantalla alternativa o con eco de teclas.
    let left = disable_raw_mode().and(execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    ));
    let result = left.map_err(Into::into).and_then(|()| f());
    let back = enable_raw_mode()
        .and(execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste
        ))
        .and(term.clear());
    let result = result?;
    back?;
    Ok(result)
}

/// Las últimas `h` líneas con contenido de una pantalla capturada.
fn tail(screen: &str, h: usize) -> String {
    let mut lines: Vec<&str> = screen.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines[lines.len().saturating_sub(h)..].join("\n")
}

/// Lo que vio el observador en una pasada.
#[derive(Default)]
struct Pass {
    /// `None` si no se pudo preguntar a tmux: se quedan los de antes.
    snaps: Option<HashMap<String, Snapshot>>,
    /// Solo si el historial cambió.
    stats: Option<Stats>,
    /// Solo si se volvieron a leer.
    limits: Option<Vec<Usage>>,
    /// Avisos de agentes que terminaron o necesitan atención.
    flashes: Vec<String>,
}

impl Pass {
    /// Junta una pasada que la TUI no llegó a tomar con la siguiente.
    fn after(mut self, old: Pass) -> Pass {
        self.flashes = [old.flashes, self.flashes].concat();
        self.stats = self.stats.or(old.stats);
        self.limits = self.limits.or(old.limits);
        self.snaps = self.snaps.or(old.snaps);
        self
    }
}

/// Lo que el observador recuerda entre pasadas.
#[derive(Default)]
struct Watch {
    tracker: Tracker,
    status: HashMap<String, Status>,
    history: history::Tail,
    limits: limits::Cache,
    /// Día de las últimas estadísticas: al cambiar se mueve el gráfico.
    day: Option<NaiveDate>,
    /// Si la pasada anterior no pudo preguntar a tmux: se avisa una vez.
    failing: bool,
}

impl Watch {
    fn look(&mut self) -> Option<Pass> {
        let state = State::load().ok()?;
        // Si tmux falló, mejor saltar esta pasada que dar a todos por
        // terminados, pero que se sepa.
        let snaps = match ops::observe(&state, &mut self.tracker) {
            Ok(snaps) => snaps,
            Err(e) => {
                let first = !std::mem::replace(&mut self.failing, true);
                return first.then(|| Pass {
                    flashes: vec![format!("No pude leer los paneles: {e:#}")],
                    ..Pass::default()
                });
            }
        };
        self.failing = false;
        // Avisar desde aquí y no desde la TUI hace que lleguen también
        // mientras estás dentro de un agente (la TUI está suspendida).
        let mut flashes = Vec::new();
        for (msg, finished) in transitions(&state, &self.status, &snaps) {
            notify(&msg);
            flashes.push(msg);
            if let Some(Err(e)) = finished.map(|id| ops::finish_turn(&id)) {
                flashes.push(format!("Error: {e:#}"));
            }
        }
        self.status = snaps.iter().map(|(id, s)| (id.clone(), s.status)).collect();
        let today = Local::now().date_naive();
        let changed = self.history.refresh() || self.day != Some(today);
        self.day = Some(today);
        let stats = changed.then(|| self.history.stats(today));
        Some(Pass {
            snaps: Some(snaps),
            stats,
            limits: None,
            flashes,
        })
    }
}

/// Avisos por los cambios de estado de `old` a `new`, cada uno con el agente
/// cuya tarea hay que pasar a revisión, si la tiene.
fn transitions(
    state: &State,
    old: &HashMap<String, Status>,
    new: &HashMap<String, Snapshot>,
) -> Vec<(String, Option<String>)> {
    let mut events = Vec::new();
    for a in &state.agents {
        let (Some(&old), Some(new)) = (old.get(&a.id), new.get(&a.id)) else {
            continue;
        };
        match (old, new.status) {
            (o, Status::Blocked) if o != Status::Blocked => {
                events.push((format!("{} necesita tu atención", a.name), None));
            }
            (Status::Working, Status::Idle | Status::Dead) => {
                // Sin tarea no hay nada que pasar a revisión.
                let id = a.task.is_some().then(|| a.id.clone());
                events.push((format!("{} terminó", a.name), id));
            }
            _ => {}
        }
    }
    events
}

/// Hilo que mira los paneles de tmux (capturas y `ps`) y el historial cada
/// `REFRESH`, o antes si se le pide, para que eso no frene el dibujo ni las
/// teclas. Solo guarda la última pasada: mientras la TUI está suspendida no se
/// acumulan capturas.
struct Observer {
    wake: Sender<()>,
    latest: Arc<Mutex<Option<Pass>>>,
    thread: JoinHandle<()>,
}

impl Observer {
    /// Con `look_now`, la primera pasada se hace aquí, para que el primer
    /// cuadro ya la tenga. Al reiniciarlo tras un pánico no: si se repite,
    /// en este hilo tumbaría la TUI; en el suyo solo lo vuelve a reiniciar.
    fn spawn(status: HashMap<String, Status>, look_now: bool) -> Self {
        let mut watch = Watch {
            status,
            ..Watch::default()
        };
        let latest = Arc::new(Mutex::new(if look_now { watch.look() } else { None }));
        let (wake, wakes) = mpsc::channel();
        let slot = Arc::clone(&latest);
        let thread = std::thread::Builder::new()
            .name(OBSERVER.into())
            .spawn(move || {
                let mut wait = look_now;
                loop {
                    if wait {
                        match wakes.recv_timeout(REFRESH) {
                            Ok(()) => while wakes.try_recv().is_ok() {},
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    wait = true;
                    let publish = |pass: Pass| {
                        let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
                        *slot = Some(match slot.take() {
                            Some(old) => pass.after(old),
                            None => pass,
                        });
                    };
                    if let Some(pass) = watch.look() {
                        publish(pass);
                    }
                    // Aparte y después de los paneles: puede tardar por la red
                    // y no debe retrasar los avisos. Tampoco en `look`: la
                    // primera pasada se hace antes del primer cuadro.
                    if watch.limits.refresh() {
                        publish(Pass {
                            limits: Some(watch.limits.usage().to_vec()),
                            ..Pass::default()
                        });
                    }
                }
            })
            // Lo mismo que haría `thread::spawn`.
            .expect("no pude lanzar el observador");
        Self {
            wake,
            latest,
            thread,
        }
    }

    fn take(&self) -> Option<Pass> {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn wake(&self) {
        let _ = self.wake.send(());
    }

    /// El hilo solo termina antes que `Observer` si entró en pánico.
    fn died(&self) -> bool {
        self.thread.is_finished()
    }
}

/// Lo que queda por hacer en la TUI cuando termina una acción.
enum Done {
    Msg(String),
    /// Se lanzó un agente: se selecciona.
    Agent {
        id: String,
        msg: String,
    },
    Quiet,
}

type Job = Box<dyn FnOnce() -> Result<Done> + Send>;

/// Hilo que ejecuta, de una en una, las acciones que hablan con tmux (lanzar,
/// enviar, asignar…), para que la TUI no se congele mientras.
struct Actions {
    /// Enviadas y todavía sin resultado.
    pending: usize,
    jobs: Option<Sender<(Job, Option<Mode>)>>,
    results: Receiver<(Result<Done>, Option<Mode>)>,
    thread: Option<JoinHandle<()>>,
}

impl Actions {
    fn spawn() -> Self {
        let (jobs, todo) = mpsc::channel::<(Job, Option<Mode>)>();
        let (done, results) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(ACTIONS.into())
            .spawn(move || {
                for (job, retry) in todo {
                    // Un pánico es un error más: el hilo sigue y lo escrito vuelve.
                    let result = std::panic::catch_unwind(AssertUnwindSafe(job))
                        .unwrap_or_else(|p| Err(anyhow!("la acción falló: {}", panic_text(&*p))));
                    if done.send((result, retry)).is_err() {
                        return;
                    }
                }
            })
            .expect("no pude lanzar el hilo de acciones");
        Self {
            pending: 0,
            jobs: Some(jobs),
            results,
            thread: Some(thread),
        }
    }

    /// Si falla, `retry` es el formulario al que volver con lo escrito.
    fn run(&mut self, retry: Option<Mode>, job: impl FnOnce() -> Result<Done> + Send + 'static) {
        if let Some(jobs) = &self.jobs {
            if jobs.send((Box::new(job), retry)).is_ok() {
                self.pending += 1;
            }
        }
    }

    /// Espera a que terminen las acciones pendientes, salvo que `abandon`
    /// diga que ya no: si tmux se colgara, la espera no tendría fin.
    fn wait(&mut self, mut abandon: impl FnMut() -> bool) {
        self.jobs = None;
        while self.pending > 0 {
            match self.results.recv_timeout(Duration::from_millis(100)) {
                Ok(_) => self.pending -= 1,
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) if abandon() => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Si se pulsó q, Esc o Ctrl-C (para dejar de esperar al salir).
fn abandon() -> bool {
    if !event::poll(Duration::ZERO).unwrap_or(false) {
        return false;
    }
    let Ok(Event::Key(key)) = event::read() else {
        return false;
    };
    key.kind == KeyEventKind::Press
        && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

/// El mensaje de un pánico, si es texto.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("pánico")
}

/// Muestra un aviso del sistema. Los lanza, de uno en uno, un único hilo que
/// espera a cada proceso (para que no quede zombi) sin frenar al observador.
fn notify(msg: &str) {
    static QUEUE: Mutex<Option<Sender<String>>> = Mutex::new(None);
    let mut queue = QUEUE.lock().unwrap_or_else(PoisonError::into_inner);
    let msg = match queue.as_ref().map(|tx| tx.send(msg.to_string())) {
        Some(Ok(())) => return,
        Some(Err(e)) => e.0,
        None => msg.to_string(),
    };
    // Sin hilo todavía, o murió: se lanza otro. Si no se puede, se intentará
    // con el próximo aviso.
    let (tx, rx) = mpsc::channel::<String>();
    let spawned = std::thread::Builder::new()
        .name("avisos".into())
        .spawn(move || {
            for msg in rx {
                show_notice(&msg);
            }
        });
    if spawned.is_ok() && tx.send(msg).is_ok() {
        *queue = Some(tx);
    }
}

/// Cuánto puede tardar un aviso antes de darlo por colgado (notify-send
/// espera hasta 25 s si nadie atiende las notificaciones).
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

fn show_notice(msg: &str) {
    // Los dos a la vez, para que uno colgado no retrase al otro.
    let desktop = notify_command(msg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let phone = ntfy::publish(msg);
    if let Ok(child) = desktop {
        wait_or_kill(child, NOTICE_TIMEOUT);
    }
    if let Some(mut child) = phone {
        // curl ya corta a los 10 s (`-m`); esto es por si acaso.
        let stderr = child.stderr.take();
        ntfy::check(stderr, wait_or_kill(child, NTFY_TIMEOUT));
    }
}

/// Lo que tarda como mucho un aviso a ntfy.
const NTFY_TIMEOUT: Duration = Duration::from_secs(12);

/// Espera a `child` hasta `timeout`; si se pasa, lo mata para que no retrase
/// a los siguientes. Cómo terminó, o `None` si hubo que matarlo.
fn wait_or_kill(mut child: Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if start.elapsed() <= timeout => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn notify_command(msg: &str) -> Command {
    // El mensaje va como argumento y no dentro del AppleScript, para no
    // tener que escaparlo. El título va antes: si el mensaje empieza con `-`
    // (el nombre del agente), osascript ya no lo toma por una opción.
    let mut cmd = Command::new("osascript");
    cmd.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        "pastor",
        msg,
    ]);
    cmd
}

#[cfg(not(target_os = "macos"))]
fn notify_command(msg: &str) -> Command {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "pastor", "--", "pastor", msg]);
    cmd
}

fn popup(f: &mut Frame, title: &str, w: u16, h: u16) -> Rect {
    let area = f.area();
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(title.to_string());
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

fn stat(f: &mut Frame, area: Rect, name: &str, value: String, color: Color) {
    let p = Paragraph::new(vec![
        Line::from(Span::styled(
            value,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        Line::from(name.to_string().dark_gray()),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(p, area);
}

fn label(name: &str, _active: bool) -> String {
    format!(" {name:>11}: ")
}

fn label_style(active: bool) -> Style {
    if active {
        Style::default().fg(ACCENT).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Ancho de un modal: `w` columnas o lo que quepa en pantalla.
fn modal_width(f: &Frame, w: u16) -> u16 {
    w.min(f.area().width.saturating_sub(2))
}

/// Líneas de contenido que caben en un modal, dejando `pad` líneas de margen.
fn modal_room(f: &Frame, pad: usize) -> usize {
    (f.area().height as usize).saturating_sub(4 + pad).max(1)
}

/// Un campo de formulario partido en varias líneas para que quepa en un modal
/// de ancho `w`; las líneas siguientes quedan alineadas con el valor.
fn field_lines(name: &str, value: &str, active: bool, w: u16) -> Vec<Line<'static>> {
    // Bordes del modal + hueco para el cursor.
    let avail = (w as usize).saturating_sub(LABEL_W + 3).max(1);
    let mut chunks: Vec<String> = Vec::new();
    for line in value.split('\n') {
        let mut rest = line;
        loop {
            let fits = text::fit(rest, avail);
            if fits == rest.len() {
                break;
            }
            // Corta después del último espacio que quepa; si no hay, a media palabra.
            let cut = rest[..fits].rfind(' ').map_or(fits, |i| i + 1);
            chunks.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        chunks.push(rest.to_string());
    }
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let head = if i == 0 {
                Span::styled(label(name, active), label_style(active))
            } else {
                Span::raw(" ".repeat(LABEL_W))
            };
            let mut spans = vec![head, Span::raw(chunk)];
            if active && i == n - 1 {
                spans.push(Span::styled("█", Style::default().fg(ACCENT)));
            }
            Line::from(spans)
        })
        .collect()
}

/// Las últimas `n` líneas (donde está el cursor) si no caben todas.
fn keep_tail(mut lines: Vec<Line<'static>>, n: usize) -> Vec<Line<'static>> {
    let n = n.max(1);
    if lines.len() > n {
        // La primera línea visible conserva la etiqueta del campo.
        let head = lines[0].spans.first().cloned();
        lines.drain(..lines.len() - n);
        if let (Some(head), Some(first)) = (head, lines[0].spans.first_mut()) {
            *first = head;
        }
    }
    lines
}

fn edit(s: &mut String, key: &KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('u') if ctrl => s.clear(),
        KeyCode::Char(c) if !ctrl && !alt => s.push(c),
        KeyCode::Backspace => {
            s.pop();
        }
        _ => {}
    }
}

fn step(cur: Option<usize>, n: usize, d: isize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let i = cur.unwrap_or(0) as isize + d;
    Some(i.clamp(0, n as isize - 1) as usize)
}

fn clamp(sel: &mut ListState, n: usize) {
    match sel.selected() {
        _ if n == 0 => sel.select(None),
        None => sel.select(Some(0)),
        Some(i) if i >= n => sel.select(Some(n - 1)),
        _ => {}
    }
}

fn kind_color(k: Kind) -> Color {
    match k {
        Kind::Claude => Color::Rgb(217, 119, 87),
        Kind::Codex => Color::Rgb(16, 163, 127),
        Kind::Opencode => Color::Cyan,
    }
}

/// Una barra de `width` celdas llena según `pct`.
fn meter(pct: f64, width: usize) -> String {
    let full = ((pct / 100.0) * width as f64).round() as usize;
    let full = full.min(width);
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

fn limit_color(left_pct: f64) -> Color {
    match left_pct {
        p if p < 10.0 => Color::Red,
        p if p < 30.0 => Color::Yellow,
        _ => Color::Green,
    }
}

fn task_color(s: TaskStatus) -> Color {
    match s {
        TaskStatus::Todo => Color::Gray,
        TaskStatus::InProgress => Color::Green,
        TaskStatus::Review => Color::Yellow,
        TaskStatus::Done => Color::DarkGray,
    }
}

fn default_cwd() -> String {
    std::env::current_dir().map_or_else(|_| String::new(), |p| tilde(&p))
}

fn tilde(p: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

/// Sin directorio no se lanza nada: el de la TUI podría ser otro proyecto.
const NO_CWD: &str = "Error: falta el directorio";

/// La ruta escrita en un formulario, con `~` expandido; `None` si está vacía.
fn expand(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    // Solo `~` y `~/…`: `~ana/…` (el home de otro) se deja tal cual.
    let rest = s
        .strip_prefix('~')
        .filter(|r| r.is_empty() || r.starts_with('/'));
    Some(match (rest, dirs::home_dir()) {
        _ if s.is_empty() => return None,
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snaps(list: &[(&str, Status)]) -> HashMap<String, Snapshot> {
        list.iter()
            .map(|&(id, status)| {
                let snap = Snapshot {
                    status,
                    screen: String::new(),
                    term: None,
                };
                (id.to_string(), snap)
            })
            .collect()
    }

    fn statuses(list: &[(&str, Status)]) -> HashMap<String, Status> {
        list.iter().map(|&(id, s)| (id.to_string(), s)).collect()
    }

    #[test]
    fn un_aviso_colgado_se_mata() {
        let spawn = |secs: &str| Command::new("sleep").arg(secs).spawn().unwrap();
        assert!(wait_or_kill(spawn("0"), Duration::from_secs(5)).is_some());
        let start = Instant::now();
        assert!(wait_or_kill(spawn("30"), Duration::from_millis(200)).is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn rutas_de_los_formularios() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand("  "), None);
        assert_eq!(expand("~"), Some(home.clone()));
        assert_eq!(expand(" ~/web "), Some(home.join("web")));
        assert_eq!(expand("/tmp/x"), Some(PathBuf::from("/tmp/x")));
        assert_eq!(expand("~ana/web"), Some(PathBuf::from("~ana/web")));
    }

    #[test]
    fn editar_un_campo() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let mut s = String::from("añ");
        edit(&mut s, &key(KeyCode::Char('🦀'), KeyModifiers::NONE));
        edit(&mut s, &key(KeyCode::Backspace, KeyModifiers::NONE));
        edit(&mut s, &key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s, "a");
        // Ctrl+letra y Alt+letra no escriben la letra; Ctrl+U borra todo.
        edit(&mut s, &key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        edit(&mut s, &key(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(s, "a");
        edit(&mut s, &key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(s, "");
        edit(&mut s, &key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s, "");
    }

    #[test]
    fn los_campos_caben_en_el_modal_con_emoji() {
        let value = "✔️ hecho ".repeat(20) + &"👨‍👩‍👧".repeat(20);
        let lines = field_lines("Mensaje", &value, false, 40);
        assert!(lines.len() > 1);
        // Lo que mide ratatui al pintar, sin el borde ni el hueco del cursor.
        assert!(lines.iter().all(|l| l.width() <= 40 - 3));
        let joined: String = lines.iter().map(|l| l.spans[1].content.as_ref()).collect();
        assert_eq!(joined, value);
    }

    #[test]
    fn avisa_al_terminar_y_al_bloquearse() {
        let mut state = State::default();
        let a1 = state.add_agent(Kind::Claude, PathBuf::from("/tmp/web"), None);
        state.add_agent(Kind::Claude, PathBuf::from("/tmp/api"), None);
        state.agents[0].task = Some("t1".into());
        let old = statuses(&[("a1", Status::Working), ("a2", Status::Idle)]);
        let new = snaps(&[("a1", Status::Idle), ("a2", Status::Blocked)]);
        assert_eq!(
            transitions(&state, &old, &new),
            [
                (format!("{} terminó", a1.name), Some("a1".to_string())),
                ("api necesita tu atención".to_string(), None),
            ]
        );
        // Sin estado anterior (recién arrancado) no hay avisos.
        assert!(transitions(&state, &HashMap::new(), &new).is_empty());
    }

    #[test]
    fn sin_tarea_no_hay_nada_que_pasar_a_revision() {
        let mut state = State::default();
        state.add_agent(Kind::Claude, PathBuf::from("/tmp/web"), None);
        let old = statuses(&[("a1", Status::Working)]);
        let new = snaps(&[("a1", Status::Dead)]);
        assert_eq!(transitions(&state, &old, &new)[0].1, None);
    }

    #[test]
    fn una_pasada_no_tomada_no_pierde_avisos_ni_estadisticas() {
        let old = Pass {
            stats: Some(Stats::default()),
            flashes: vec!["a terminó".into()],
            ..Pass::default()
        };
        let new = Pass {
            flashes: vec!["b terminó".into()],
            ..Pass::default()
        };
        let p = new.after(old);
        assert_eq!(p.flashes, ["a terminó", "b terminó"]);
        assert!(p.stats.is_some());
    }
}
