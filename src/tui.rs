//! Interfaz de terminal: pestañas de Agentes, Tareas y Dashboard.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    BarChart, Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::agent::{Kind, Status};
use crate::ops::{self, AssignTarget, Snapshot, Tracker};
use crate::store::{Agent, State, Task, TaskStatus};

const REFRESH: Duration = Duration::from_secs(1);
const SELECTED: Color = Color::Rgb(45, 45, 60);
const ACCENT: Color = Color::Magenta;

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = App::new().run(&mut terminal);
    ratatui::restore();
    result
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Agents,
    Tasks,
    Dashboard,
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
    flash: Option<(String, Instant)>,
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
            flash: None,
            last_refresh: Instant::now(),
            quit: false,
        }
    }

    fn run(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        self.refresh();
        while !self.quit {
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key, term);
                    }
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

    fn on_normal(&mut self, key: KeyEvent, term: &mut DefaultTerminal) -> Result<()> {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('1') => self.tab = Tab::Agents,
            KeyCode::Char('2') => self.tab = Tab::Tasks,
            KeyCode::Char('3') => self.tab = Tab::Dashboard,
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Agents => Tab::Tasks,
                    Tab::Tasks => Tab::Dashboard,
                    Tab::Dashboard => Tab::Agents,
                }
            }
            _ => match self.tab {
                Tab::Agents => self.on_agents(key, term)?,
                Tab::Tasks => self.on_tasks(key)?,
                Tab::Dashboard => {}
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
                    let t = State::update(|s| {
                        Ok(s.add_task(title.trim().to_string(), desc.trim().to_string()))
                    })?;
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
        }
        self.draw_footer(f, footer);
        self.draw_modal(f);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let [logo, tabs, summary] = Layout::horizontal([
            Constraint::Length(9),
            Constraint::Min(0),
            Constraint::Length(30),
        ])
        .areas(area);
        f.render_widget(
            Span::styled(
                " pastor ",
                Style::default().fg(Color::Black).bg(ACCENT).bold(),
            ),
            logo,
        );
        let idx = match self.tab {
            Tab::Agents => 0,
            Tab::Tasks => 1,
            Tab::Dashboard => 2,
        };
        let t = Tabs::new([" 1 Agentes ", " 2 Tareas ", " 3 Dashboard "])
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
                format!("{} {}  ", st.icon(), n),
                Style::default().fg(st.color()),
            ));
        }
        f.render_widget(Line::from(spans).alignment(Alignment::Right), summary);
    }

    fn draw_agents(&mut self, f: &mut Frame, area: Rect) {
        let [left, right] =
            Layout::horizontal([Constraint::Length(44), Constraint::Min(0)]).areas(area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Agentes ");

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
                        Span::styled(format!(" {} ", st.icon()), Style::default().fg(st.color())),
                        Span::styled(
                            format!("{:<16}", truncate(&a.name, 16)),
                            Style::default().bold(),
                        ),
                        Span::styled(
                            format!(" {:<9}", a.kind.to_string()),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{:<11}", st.label()),
                            Style::default().fg(st.color()),
                        ),
                        Span::styled(
                            if term_open { "⊞" } else { " " },
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
                let [l, r] =
                    Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .areas(right);
                let title = match (&a.term_cmd, t.dead) {
                    (_, true) => " terminal · cerrada ".to_string(),
                    (Some(c), false) => format!(" terminal · {c} "),
                    (None, false) => " terminal ".to_string(),
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
            .title(format!(" {} · {} · {} ", a.name, a.kind, tilde(&a.cwd)));
        if let Some(t) = a.task.as_ref().and_then(|t| self.state.find_task(t).ok()) {
            block = block.title_bottom(format!(" tarea {}: {} ", t.id, truncate(&t.title, 50)));
        }
        f.render_widget(Paragraph::new(text).block(block), right);
    }

    fn draw_tasks(&mut self, f: &mut Frame, area: Rect) {
        let [top, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(7)]).areas(area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Tareas ");

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
                                    format!("{} ", st.icon()),
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
                            t.status.label(),
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
                    Constraint::Length(12),
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
            "agentes",
            self.state.agents.len().to_string(),
            Color::White,
        );
        stat(
            f,
            b,
            "trabajando",
            (count(Status::Working) + count(Status::Starting)).to_string(),
            Status::Working.color(),
        );
        stat(
            f,
            c,
            "esperándote",
            count(Status::Blocked).to_string(),
            Status::Blocked.color(),
        );
        stat(
            f,
            d,
            "inactivos",
            count(Status::Idle).to_string(),
            Status::Idle.color(),
        );
        stat(
            f,
            e,
            "terminados",
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
            stat(f, area, st.label(), tcount(st).to_string(), task_color(st));
        }
        stat(f, e, "completado", format!("{pct}%"), ACCENT);

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(r3);
        let mut items: Vec<ListItem> = Vec::new();
        for a in &self.state.agents {
            if self.status(&a.id) == Status::Blocked {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(" ▲ ", Style::default().fg(Color::Yellow)),
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
                Span::styled(" ◆ ", Style::default().fg(task_color(TaskStatus::Review))),
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
                    .title(" Necesita tu atención "),
            ),
            left,
        );

        let data: Vec<(&str, u64)> = Kind::ALL
            .iter()
            .map(|k| {
                let label = match k {
                    Kind::Claude => "claude",
                    Kind::Codex => "codex",
                    Kind::Opencode => "opencode",
                };
                (
                    label,
                    self.state.agents.iter().filter(|a| a.kind == *k).count() as u64,
                )
            })
            .collect();
        let chart = BarChart::default()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Agentes por tipo "),
            )
            .data(data.as_slice())
            .bar_width(9)
            .bar_gap(2)
            .bar_style(Style::default().fg(ACCENT))
            .value_style(Style::default().fg(Color::Black).bg(ACCENT).bold());
        f.render_widget(chart, right);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if let Some((msg, at)) = &self.flash {
            if at.elapsed() < Duration::from_secs(5) {
                f.render_widget(
                    Span::styled(format!(" {msg}"), Style::default().fg(Color::Yellow)),
                    area,
                );
                return;
            }
        }
        let help = match (&self.mode, self.tab) {
            (Mode::Normal, Tab::Agents) => "↑↓ mover · ⏎ entrar · n nuevo · s mensaje · t terminal · T cerrar terminal · x terminar · q salir",
            (Mode::Normal, Tab::Tasks) => "↑↓ mover · n nueva · ⏎ asignar · espacio avanzar estado · d hecha · g ir al agente · x borrar",
            (Mode::Normal, Tab::Dashboard) => "1-3 pestañas · q salir",
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
                let inner = popup(f, " Nuevo agente ", 70, 9);
                let mut kinds = vec![Span::styled(
                    label("Tipo", form.field == 0),
                    label_style(form.field == 0),
                )];
                for (i, k) in Kind::ALL.iter().enumerate() {
                    let style = if i == form.kind {
                        Style::default().fg(Color::Black).bg(ACCENT).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    kinds.push(Span::styled(format!(" {k} "), style));
                    kinds.push(Span::raw(" "));
                }
                if form.field == 0 {
                    kinds.push(Span::styled("← →", Style::default().fg(Color::DarkGray)));
                }
                let lines = vec![
                    Line::from(kinds),
                    Line::raw(""),
                    field_line("Directorio", &form.cwd, form.field == 1),
                    field_line("Nombre", &form.name, form.field == 2),
                    field_line("Mensaje", &form.prompt, form.field == 3),
                ];
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::Send { agent, input } => {
                let inner = popup(
                    f,
                    &format!(" Mensaje para {} ", self.agent_name(agent)),
                    80,
                    4,
                );
                f.render_widget(
                    Paragraph::new(vec![Line::raw(""), field_line("Mensaje", input, true)]),
                    inner,
                );
            }
            Mode::Term { agent, input } => {
                let inner = popup(
                    f,
                    &format!(" Terminal junto a {} ", self.agent_name(agent)),
                    80,
                    5,
                );
                let lines = vec![
                    Line::raw(""),
                    field_line("Comando", input, true),
                    Line::from("  Déjalo vacío para abrir solo una terminal.".dark_gray()),
                ];
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::NewTask { title, desc, field } => {
                let inner = popup(f, " Nueva tarea ", 80, 5);
                let lines = vec![
                    Line::raw(""),
                    field_line("Título", title, *field == 0),
                    field_line("Descripción", desc, *field == 1),
                ];
                f.render_widget(Paragraph::new(lines), inner);
            }
            Mode::Assign(form) => {
                let h = form.options.len() as u16 + 6;
                let inner = popup(f, &format!(" Asignar {} ", form.task), 64, h);
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
                lines.push(field_line(
                    "Directorio",
                    &form.cwd,
                    matches!(form.options[form.sel], AssignOpt::New(_)),
                ));
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
    execute!(io::stdout(), LeaveAlternateScreen)?;
    let result = f();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
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

fn field_line(name: &str, value: &str, active: bool) -> Line<'static> {
    let mut spans = vec![
        Span::styled(label(name, active), label_style(active)),
        Span::raw(value.to_string()),
    ];
    if active {
        spans.push(Span::styled("█", Style::default().fg(ACCENT)));
    }
    Line::from(spans)
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
