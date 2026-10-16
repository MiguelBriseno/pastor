//! Interfaz de terminal: pestañas de Agentes, Tareas, Dashboard e Historial.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
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
    Bar, BarChart, BarGroup, Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::agent::{Kind, Status};
use crate::history::{self, Stats};
use crate::icons::{self, pad};
use crate::ops::{self, AssignTarget, Snapshot, Tracker};
use crate::store::{Agent, State, Task, TaskStatus};

const REFRESH: Duration = Duration::from_secs(1);
const SELECTED: Color = Color::Rgb(45, 45, 60);
const ACCENT: Color = Color::Magenta;
/// Columnas de la etiqueta de un campo (`" {name:>11}: "`).
const LABEL_W: usize = 14;

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
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

struct App {
    tab: Tab,
    mode: Mode,
    state: State,
    snaps: HashMap<String, Snapshot>,
    tracker: Tracker,
    agent_sel: ListState,
    task_sel: TableState,
    /// Tamaños ajustables de los paneles.
    list_width: u16,
    term_pct: u16,
    desc_height: u16,
    flash: Option<(String, Instant)>,
    /// Tamaño de history.jsonl al calcular `stats`; se recalcula si crece.
    history_len: u64,
    stats: Stats,
    last_refresh: Instant,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            tab: Tab::Agents,
            mode: Mode::Normal,
            state: State::default(),
            snaps: HashMap::new(),
            tracker: Tracker::default(),
            agent_sel: ListState::default(),
            task_sel: TableState::default(),
            list_width: 44,
            term_pct: 40,
            desc_height: 7,
            flash: None,
            history_len: u64::MAX,
            stats: Stats::default(),
            last_refresh: Instant::now(),
            quit: false,
        }
    }

    fn run(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        self.refresh();
        while !self.quit {
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key, term),
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
            if self.last_refresh.elapsed() >= REFRESH {
                self.refresh();
            }
        }
        Ok(())
    }

    // ---------- estado ----------

    fn refresh(&mut self) {
        match State::load() {
            Ok(s) => self.state = s,
            Err(e) => self.set_flash(format!("Error: {e:#}")),
        }
        let snaps = ops::observe(&self.state, &mut self.tracker);

        let mut events = Vec::new();
        for a in &self.state.agents {
            let (Some(old), Some(new)) = (self.snaps.get(&a.id), snaps.get(&a.id)) else {
                continue;
            };
            match (old.status, new.status) {
                (o, Status::Blocked) if o != Status::Blocked => {
                    events.push((format!("{} necesita tu atención", a.name), None));
                }
                (Status::Working, Status::Idle | Status::Dead) => {
                    events.push((format!("{} terminó", a.name), a.task.clone()));
                }
                _ => {}
            }
        }
        self.snaps = snaps;

        for (msg, task) in events {
            notify(&msg);
            if let Some(t) = task {
                let in_progress = self
                    .state
                    .find_task(&t)
                    .is_ok_and(|t| t.status == TaskStatus::InProgress);
                if in_progress && ops::set_task_status(&t, TaskStatus::Review).is_ok() {
                    if let Ok(task) = self.state.find_task_mut(&t) {
                        task.status = TaskStatus::Review;
                    }
                }
            }
            self.set_flash(msg);
        }

        let len = std::fs::metadata(history::path()).map_or(0, |m| m.len());
        if len != self.history_len {
            self.history_len = len;
            self.stats = history::stats(&history::load());
        }

        clamp(&mut self.agent_sel, self.state.agents.len());
        let n = self.state.tasks.len();
        match self.task_sel.selected() {
            _ if n == 0 => self.task_sel.select(None),
            None => self.task_sel.select(Some(0)),
            Some(i) if i >= n => self.task_sel.select(Some(n - 1)),
            _ => {}
        }
        self.last_refresh = Instant::now();
    }

    fn status(&self, id: &str) -> Status {
        self.snaps.get(id).map_or(Status::Starting, |s| s.status)
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
            Mode::Assign(f) => (&mut f.cwd, false),
            Mode::Normal | Mode::Confirm { .. } => return,
        };
        if multiline {
            field.push_str(&text);
        } else {
            field.push_str(text.trim_end_matches('\n').replace('\n', " ").as_str());
        }
    }

    fn on_normal(&mut self, key: KeyEvent, term: &mut DefaultTerminal) -> Result<()> {
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
                    match ops::new_agent(kind, Some(expand(&f.cwd)), name, prompt.as_deref()) {
                        Ok(a) => {
                            self.set_flash(format!("Agente {} lanzado", a.name));
                            self.refresh();
                            let i = self.state.agents.iter().position(|x| x.id == a.id);
                            self.agent_sel.select(i);
                        }
                        Err(e) => {
                            self.mode = Mode::NewAgent(f);
                            return Err(e);
                        }
                    }
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
                        ops::send(&agent, &input)?;
                        self.set_flash(format!("Mensaje enviado a {}", self.agent_name(&agent)));
                    }
                } else {
                    edit(&mut input, &key);
                    self.mode = Mode::Send { agent, input };
                }
            }
            Mode::Term { agent, mut input } => {
                if key.code == KeyCode::Enter {
                    ops::open_term(&agent, Some(&input))?;
                    self.set_flash(format!(
                        "Terminal abierta junto a {} · ⏎ para entrar",
                        self.agent_name(&agent)
                    ));
                    self.refresh();
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
                    let t = ops::add_task(title.trim().to_string(), desc.trim().to_string())?;
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
                        AssignOpt::New(kind) => AssignTarget::New {
                            kind: *kind,
                            cwd: expand(&f.cwd),
                        },
                    };
                    match ops::assign_task(&f.task, target) {
                        Ok(a) => {
                            self.set_flash(format!("Tarea {} asignada a {}", f.task, a.name));
                            self.refresh();
                        }
                        Err(e) => {
                            self.mode = Mode::Assign(f);
                            return Err(e);
                        }
                    }
                }
                _ => {
                    edit(&mut f.cwd, &key);
                    self.mode = Mode::Assign(f);
                }
            },
            Mode::Confirm { action, .. } => {
                if key.code == KeyCode::Char('y') || key.code == KeyCode::Char('s') {
                    match action {
                        Pending::KillAgent(id) => ops::kill_agent(&id)?,
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
            Constraint::Length(logo_text.chars().count() as u16),
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
                .filter(|a| self.status(&a.id) == st)
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
        let kind_w = Kind::ALL.iter().map(|k| pad(ic.kind(*k)).chars().count()).max().unwrap_or(0);
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
                        Span::styled(format!(" {} ", ic.status(st)), Style::default().fg(st.color())),
                        Span::styled(
                            format!("{:<name_w$}", truncate(&a.name, name_w)),
                            Style::default().bold(),
                        ),
                        Span::styled(
                            format!(" {:<kind_w$}", pad(ic.kind(a.kind))),
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
                let title = match (&a.term_cmd, t.dead) {
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

        let st = self.status(&a.id);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(st.color()))
            .title(Line::from(vec![
                Span::styled(format!(" {} ", ic.status(st)), Style::default().fg(st.color())),
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
        let [r1, r2, r3] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Min(0),
        ])
        .areas(area);

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
        stat(f, e, &named(ic.done, "completado"), format!("{pct}%"), ACCENT);

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
            |d| d.with_timezone(&chrono::Local).format("%d/%m/%Y").to_string(),
        );
        let days = s.since.map_or(0, |d| (chrono::Utc::now() - d).num_days());
        let [a, b, c, d, e, g] = Layout::horizontal([Constraint::Ratio(1, 6); 6]).areas(r1);
        stat(f, a, &format!("desde (hace {days} días)"), since, ACCENT);
        stat(f, b, &named(ic.agents, "sesiones"), s.sessions.to_string(), Color::White);
        stat(f, c, &named(ic.tasks, "tareas creadas"), s.tasks_created.to_string(), Color::White);
        stat(
            f,
            d,
            &named(ic.task_status(TaskStatus::Done), "tareas hechas"),
            s.tasks_done.to_string(),
            Color::Green,
        );
        stat(f, e, "mensajes enviados", s.messages.to_string(), Color::Cyan);
        stat(f, g, "turnos de Claude", s.turns.to_string(), kind_color(Kind::Claude));

        let dash = |v: Option<i64>| v.map_or_else(|| "—".to_string(), history::human);
        let per_day = format!("{:.1}", s.sessions as f64 / days.max(1) as f64);
        let [a, b, c, d] = Layout::horizontal([Constraint::Ratio(1, 4); 4]).areas(r2);
        stat(f, a, "duración media de sesión", dash(s.avg_session_secs), Color::White);
        stat(f, b, "tiempo medio por tarea", dash(s.avg_task_secs), Color::White);
        stat(f, c, "tiempo total de agentes", history::human(s.agent_secs), Color::White);
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
        let bar_w = (left.width.saturating_sub(2) / n).saturating_sub(1).clamp(2, 8);
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
            items.push(ListItem::new(" Aún no hay sesiones registradas".dark_gray()));
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
                for (i, opt) in form.options.iter().enumerate() {
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
                lines.extend(field_lines(
                    "Directorio",
                    &form.cwd,
                    matches!(form.options[form.sel], AssignOpt::New(_)),
                    w,
                ));
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
    disable_raw_mode()?;
    execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen)?;
    let result = f();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    term.clear()?;
    result
}

/// Las últimas `h` líneas con contenido de una pantalla capturada.
fn tail(screen: &str, h: usize) -> String {
    let mut lines: Vec<&str> = screen.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines[lines.len().saturating_sub(h)..].join("\n")
}

fn notify(msg: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "pastor", "pastor", msg])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
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
        let mut rest: &[char] = &line.chars().collect::<Vec<_>>();
        while rest.len() > avail {
            // Corta después del último espacio que quepa; si no hay, a media palabra.
            let cut = rest[..avail]
                .iter()
                .rposition(|c| *c == ' ')
                .map_or(avail, |i| i + 1);
            chunks.push(rest[..cut].iter().collect());
            rest = &rest[cut..];
        }
        chunks.push(rest.iter().collect());
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
    match key.code {
        KeyCode::Char('u') if ctrl => s.clear(),
        KeyCode::Char(c) if !ctrl => s.push(c),
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

fn task_color(s: TaskStatus) -> Color {
    match s {
        TaskStatus::Todo => Color::Gray,
        TaskStatus::InProgress => Color::Green,
        TaskStatus::Review => Color::Yellow,
        TaskStatus::Done => Color::DarkGray,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
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

fn expand(s: &str) -> PathBuf {
    let s = s.trim();
    match (s.strip_prefix('~'), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(s),
    }
}
