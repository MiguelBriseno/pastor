//! El editor de archivos: a la izquierda los archivos del directorio en
//! carpetas (con lo que cambió en git), a la derecha el archivo abierto, con colores, búsqueda
//! y edición sencilla. Se abre con `e` sobre un agente o con `pastor edit`.
//!
//! Leer y guardar va en hilos aparte: con un agente de otra máquina pasa por
//! SSH y tarda, y la interfaz no debe trabarse mientras.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::{self, Buffer};
use crate::files::{Change, Entry, Listing, Opened, Source};
use crate::icons::{self, pad};
use crate::syntax::{self, Colors, Painted};
use crate::text;
use crate::tui::{edit, fit_help, ACCENT, FLASH, SELECTED};

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    List,
    File,
}

/// Lo que se escribe en el pie.
enum Prompt {
    /// El filtro de la lista: filtra mientras se escribe.
    Filter,
    Search(String),
    Goto(String),
    /// Hay cambios sin guardar y se pidió `then`.
    Discard(Then),
}

/// Lo que se hace después de guardar o descartar los cambios.
#[derive(Clone)]
enum Then {
    Close,
    Open(String),
    Reload,
}

/// Lo que devuelve un hilo de trabajo, con la vuelta que lo pidió: una
/// respuesta de una vuelta anterior ya no vale.
enum Loaded {
    List(u64, Result<Listing>),
    File(u64, Result<Opened>),
    Saved {
        /// El `file_gen` del buffer que se guardó.
        gen: u64,
        path: String,
        at: Option<u64>,
        result: Result<String>,
    },
}

/// Una fila de la lista. Sin filtro, las carpetas en árbol; filtrando, solo
/// los archivos que coinciden, con su ruta entera.
#[derive(Debug, PartialEq)]
enum Row {
    Dir {
        path: String,
        depth: usize,
        open: bool,
        /// Cuántos archivos cambiaron dentro.
        changed: usize,
    },
    /// El índice en `listing.files`.
    File { i: usize, depth: usize },
}

struct FileView {
    /// La apertura de la que salió (`file_gen` entonces): un guardado
    /// solo vale para el buffer que lo pidió.
    gen: u64,
    path: String,
    buf: Buffer,
    hash: String,
    colors: Option<Colors>,
    /// La primera línea y la primera columna que se ven.
    top: usize,
    left: usize,
    editing: bool,
    saving: bool,
}

pub struct Editor {
    source: Source,
    title: String,
    listing: Option<Listing>,
    list_error: Option<String>,
    filter: String,
    only_changed: bool,
    /// Las filas que se ven, en orden.
    shown: Vec<Row>,
    /// Cuántos archivos pasan el filtro.
    matches: usize,
    /// Las carpetas abiertas.
    expanded: HashSet<String>,
    /// Con `c`, las que estaban abiertas antes: al quitarlo se vuelve a ellas.
    expanded_before: Option<HashSet<String>>,
    list_sel: ListState,
    focus: Focus,
    file: Option<FileView>,
    /// El archivo que se está leyendo.
    opening: Option<String>,
    prompt: Option<Prompt>,
    search: String,
    /// Al guardar, lo que se hace después (si se guardó para salir), con el
    /// `gen` del buffer que se guarda: el guardado de otro no lo toca.
    after_save: Option<(u64, Then)>,
    flash: Option<(String, Instant)>,
    list_width: u16,
    /// Las líneas de texto que caben, del último cuadro: para avanzar páginas.
    page: usize,
    list_gen: u64,
    file_gen: u64,
    /// El archivo abierto (su `gen` y su versión) al pedir otro.
    open_from: Option<(u64, Option<u64>)>,
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
    pub closed: bool,
}

impl Editor {
    /// Los archivos de `source`. Si viene `open`, ese archivo ya abierto.
    pub fn new(source: Source, title: String, open: Option<String>) -> Editor {
        let (tx, rx) = mpsc::channel();
        let mut ed = Editor {
            source,
            title,
            listing: None,
            list_error: None,
            filter: String::new(),
            only_changed: false,
            shown: Vec::new(),
            matches: 0,
            expanded: HashSet::new(),
            expanded_before: None,
            list_sel: ListState::default(),
            focus: Focus::List,
            file: None,
            opening: None,
            prompt: None,
            search: String::new(),
            after_save: None,
            flash: None,
            list_width: 40,
            page: 20,
            list_gen: 0,
            file_gen: 0,
            open_from: None,
            tx,
            rx,
            closed: false,
        };
        ed.load_list();
        if let Some(path) = open {
            ed.open(path);
        }
        ed
    }

    fn spawn(&self, job: impl FnOnce() -> Loaded + Send + 'static) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    fn load_list(&mut self) {
        self.list_gen += 1;
        let (n, source) = (self.list_gen, self.source.clone());
        self.spawn(move || Loaded::List(n, source.list()));
    }

    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    /// Recoge lo que terminaron los hilos. `true` si hay que redibujar.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH)
        {
            self.flash = None;
            changed = true;
        }
        while let Ok(loaded) = self.rx.try_recv() {
            changed = true;
            match loaded {
                Loaded::List(n, _) if n != self.list_gen => {}
                Loaded::List(_, Ok(listing)) => {
                    // La primera vez, el archivo que ya se abrió.
                    let keep = match &self.listing {
                        None => self.file.as_ref().map(|f| f.path.clone()),
                        Some(_) => self.selected_key(),
                    };
                    self.listing = Some(listing);
                    self.list_error = None;
                    if self.only_changed {
                        // Lo que cambió desde que se pulsó `c`, también a la vista.
                        self.open_changed();
                    }
                    self.refilter(keep.as_deref());
                    if let Some(l) = self.listing.as_ref().filter(|l| l.truncated) {
                        self.set_flash(format!(
                            "Hay más de {} archivos: solo se listan los primeros",
                            l.files.len()
                        ));
                    }
                }
                // Con la lista ya a la vista, el error de recargarla no se
                // dibujaría: va en el pie.
                Loaded::List(_, Err(e)) if self.listing.is_some() => {
                    self.set_flash(format!("No pude recargar la lista: {e:#}"));
                }
                Loaded::List(_, Err(e)) => self.list_error = Some(format!("{e:#}")),
                Loaded::File(n, _) if n != self.file_gen => {}
                Loaded::File(n, result) => {
                    self.opening = None;
                    match result {
                        Ok(opened) => self.opened(n, opened),
                        Err(e) => self.set_flash(format!("Error: {e:#}")),
                    }
                }
                Loaded::Saved {
                    gen,
                    path,
                    at,
                    result,
                } => self.saved(gen, path, at, result),
            }
        }
        changed
    }

    fn opened(&mut self, gen: u64, o: Opened) {
        // Si mientras se leía cambiaste el que estaba abierto, esos cambios
        // no se aceptaron perder: se queda ese.
        if let Some(f) = &self.file {
            if f.buf.dirty() && self.open_from != Some((f.gen, f.buf.version())) {
                self.set_flash(format!(
                    "No abrí {}: {} tiene cambios sin guardar",
                    o.path, f.path
                ));
                return;
            }
        }
        let buf = Buffer::new(&o.text);
        let colors = if o.text.len() <= syntax::MAX_BYTES {
            Colors::new(&o.path, buf.line(0))
        } else {
            None
        };
        // Al volver a leer el mismo archivo, sin moverse de donde estabas.
        let (top, cursor) = match &self.file {
            Some(f) if f.path == o.path => (f.top, Some(f.buf.cursor)),
            _ => (0, None),
        };
        let mut view = FileView {
            gen,
            path: o.path,
            buf,
            hash: o.hash,
            colors,
            top,
            left: 0,
            editing: false,
            saving: false,
        };
        if let Some(c) = cursor {
            view.buf.goto(c);
        }
        // En la lista, el que se abrió (con sus carpetas abiertas). Al
        // volver a leer el mismo, la lista se queda como estaba.
        if self.listing.is_some() && cursor.is_none() {
            let path = view.path.clone();
            self.refilter(Some(&path));
        }
        self.file = Some(view);
        self.focus = Focus::File;
    }

    fn saved(&mut self, gen: u64, path: String, at: Option<u64>, result: Result<String>) {
        // Si se volvió a abrir mientras, el buffer de ahora no es el que se
        // guardó: su hash y su historia no se tocan.
        let current = self.file.as_ref().is_some_and(|f| f.gen == gen);
        // Lo que se pidió hacer al guardar este buffer, y solo eso.
        let then = match self.after_save.take() {
            Some((g, then)) if g == gen => Some(then),
            other => {
                self.after_save = other;
                None
            }
        };
        let Some(f) = self.file.as_mut().filter(|f| current && f.path == path) else {
            match result {
                Ok(_) => {
                    self.set_flash(format!("{path} guardado"));
                    self.load_list();
                }
                Err(e) => self.set_flash(format!("Error: {e:#}")),
            }
            return;
        };
        f.saving = false;
        match result {
            Ok(hash) => {
                f.hash = hash;
                f.buf.saved_as(at);
                // Lo que se escribió mientras se guardaba sigue sin guardar.
                let more = f.buf.dirty();
                self.set_flash(format!("{path} guardado"));
                // Las marcas de lo que cambió también.
                self.load_list();
                if let Some(then) = then {
                    if more {
                        self.after_save = Some((gen, then));
                        self.save();
                    } else {
                        self.then(then);
                    }
                }
            }
            Err(e) => self.set_flash(format!("Error: {e:#}")),
        }
    }

    // ---------- la lista ----------

    fn selected_row(&self) -> Option<&Row> {
        self.shown.get(self.list_sel.selected()?)
    }

    /// El archivo seleccionado (no si es una carpeta).
    fn selected_path(&self) -> Option<String> {
        let l = self.listing.as_ref()?;
        match self.selected_row()? {
            Row::File { i, .. } => Some(l.files[*i].path.clone()),
            Row::Dir { .. } => None,
        }
    }

    /// La ruta de la fila seleccionada, carpeta o archivo.
    fn selected_key(&self) -> Option<String> {
        let l = self.listing.as_ref()?;
        Some(row_path(&l.files, self.selected_row()?).to_string())
    }

    /// Vuelve a filtrar la lista, dejando seleccionado `keep` (un archivo o
    /// una carpeta) si sigue, con sus carpetas abiertas.
    fn refilter(&mut self, keep: Option<&str>) {
        let Some(l) = &self.listing else {
            return;
        };
        let q = self.filter.to_lowercase();
        let mut picked: Vec<(usize, (bool, usize))> = l
            .files
            .iter()
            .enumerate()
            .filter(|(_, e)| !self.only_changed || e.change.is_some())
            .filter_map(|(i, e)| fuzzy(&e.path, &q).map(|rank| (i, rank)))
            .collect();
        self.matches = picked.len();
        if q.is_empty() {
            if let Some(p) = keep {
                open_parents(&mut self.expanded, p);
            }
            let picked: Vec<usize> = picked.into_iter().map(|(i, _)| i).collect();
            self.shown = tree(&l.files, &picked, &self.expanded);
        } else {
            picked.sort_by_key(|(_, rank)| *rank);
            self.shown = picked
                .into_iter()
                .map(|(i, _)| Row::File { i, depth: 0 })
                .collect();
        }
        let at = keep.and_then(|p| self.shown.iter().position(|r| row_path(&l.files, r) == p));
        let sel = match at {
            Some(i) => Some(i),
            None if self.shown.is_empty() => None,
            None => Some(
                self.list_sel
                    .selected()
                    .unwrap_or(0)
                    .min(self.shown.len() - 1),
            ),
        };
        self.list_sel.select(sel);
    }

    /// Cambió el filtro: con algo escrito, lo primero que pasa; vacío, de
    /// vuelta al árbol con lo que estaba seleccionado.
    fn filter_changed(&mut self) {
        if self.filter.is_empty() {
            let keep = self.selected_key();
            self.refilter(keep.as_deref());
        } else {
            self.list_sel.select(Some(0));
            self.refilter(None);
        }
    }

    /// Abre las carpetas con archivos cambiados.
    fn open_changed(&mut self) {
        if let Some(l) = &self.listing {
            for e in l.files.iter().filter(|e| e.change.is_some()) {
                open_parents(&mut self.expanded, &e.path);
            }
        }
    }

    /// Abre o cierra la carpeta `path`.
    fn fold(&mut self, path: &str, open: bool) {
        if open {
            self.expanded.insert(path.to_string());
        } else {
            self.expanded.remove(path);
        }
        self.refilter(Some(path));
    }

    /// Selecciona la carpeta que contiene la fila seleccionada.
    fn select_parent(&mut self) {
        let Some(key) = self.selected_key() else {
            return;
        };
        let Some((parent, _)) = key.rsplit_once('/') else {
            return;
        };
        if let Some(i) = self
            .shown
            .iter()
            .position(|r| matches!(r, Row::Dir { path, .. } if path == parent))
        {
            self.list_sel.select(Some(i));
        }
    }

    fn move_list(&mut self, d: isize) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() - 1;
        let at = self.list_sel.selected().unwrap_or(0);
        self.list_sel
            .select(Some(at.saturating_add_signed(d).min(last)));
    }

    // ---------- acciones ----------

    fn dirty(&self) -> bool {
        self.file.as_ref().is_some_and(|f| f.buf.dirty())
    }

    /// Hace `then`, preguntando antes si se perderían cambios.
    fn ask(&mut self, then: Then) {
        let same =
            matches!(&then, Then::Open(p) if self.file.as_ref().is_some_and(|f| &f.path == p));
        if same {
            self.focus = Focus::File;
        } else if self.dirty() {
            self.prompt = Some(Prompt::Discard(then));
        } else {
            self.then(then);
        }
    }

    fn then(&mut self, then: Then) {
        match then {
            Then::Close => self.closed = true,
            Then::Open(path) => self.open(path),
            Then::Reload => {
                if let Some(path) = self.file.as_ref().map(|f| f.path.clone()) {
                    self.open(path);
                }
            }
        }
    }

    fn open(&mut self, path: String) {
        // Lo que había al pedirlo: perderlo ya se aceptó (o no había cambios).
        // Con `checkpoint`, lo que se escriba mientras es un cambio aparte (no
        // se suma al último) y la versión deja de ser esta.
        self.open_from = self.file.as_mut().map(|f| (f.gen, f.buf.checkpoint()));
        self.file_gen += 1;
        self.opening = Some(path.clone());
        let (n, source) = (self.file_gen, self.source.clone());
        self.spawn(move || {
            let read = source.read(&path);
            // Las gramáticas, en este hilo: cargarlas traba un momento.
            syntax::preload();
            Loaded::File(n, read)
        });
    }

    fn save(&mut self) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        if f.saving {
            return;
        }
        if !f.buf.dirty() {
            self.set_flash("No hay cambios que guardar");
            return;
        }
        f.saving = true;
        let at = f.buf.checkpoint();
        let (path, text, hash) = (f.path.clone(), f.buf.text(), f.hash.clone());
        let gen = f.gen;
        let source = self.source.clone();
        self.set_flash(format!("Guardando {path}…"));
        self.spawn(move || Loaded::Saved {
            result: source.write(&path, &text, &hash),
            gen,
            path,
            at,
        });
    }

    fn find(&mut self, back: bool) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        match f.buf.find(&self.search, back) {
            Some(pos) => f.buf.goto(pos),
            None => {
                let msg = format!("No encontré «{}»", self.search);
                self.set_flash(msg);
            }
        }
    }

    // ---------- teclado ----------

    pub fn on_paste(&mut self, text: &str) {
        match &mut self.prompt {
            Some(Prompt::Filter) => {
                self.filter.push_str(&text.replace('\n', ""));
                self.filter_changed();
            }
            Some(Prompt::Search(s) | Prompt::Goto(s)) => s.push_str(&text.replace('\n', " ")),
            Some(Prompt::Discard(_)) => {}
            None => {
                if let Some(f) = self.file.as_mut().filter(|f| f.editing) {
                    let row = f.buf.insert(text);
                    changed(f, row);
                    f.buf.seal();
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if let Some(p) = self.prompt.take() {
            self.on_prompt(p, key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            match key.code {
                KeyCode::Char('s') => self.save(),
                KeyCode::Char('c' | 'q') => self.ask(Then::Close),
                KeyCode::Char('z') if self.focus == Focus::File => self.history(true),
                KeyCode::Char('y') if self.focus == Focus::File => self.history(false),
                KeyCode::Char('d') if self.focus == Focus::File => self.scroll(1),
                KeyCode::Char('u') if self.focus == Focus::File => self.scroll(-1),
                _ => {}
            }
            return;
        }
        match self.focus {
            Focus::List => self.on_list(key),
            Focus::File if self.file.as_ref().is_some_and(|f| f.editing) => self.on_edit(key),
            Focus::File => self.on_view(key),
        }
    }

    fn history(&mut self, undo: bool) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        let row = if undo { f.buf.undo() } else { f.buf.redo() };
        match row {
            Some(row) => changed(f, row),
            None if undo => self.set_flash("No hay nada que deshacer"),
            None => self.set_flash("No hay nada que rehacer"),
        }
    }

    /// Media página arriba o abajo.
    fn scroll(&mut self, dir: isize) {
        let half = (self.page / 2).max(1) as isize;
        if let Some(f) = self.file.as_mut() {
            f.buf.vertical(dir * half);
        }
    }

    fn on_prompt(&mut self, p: Prompt, key: KeyEvent) {
        match p {
            Prompt::Filter => {
                match key.code {
                    KeyCode::Esc => self.filter.clear(),
                    KeyCode::Enter | KeyCode::Down | KeyCode::Up | KeyCode::Tab => {
                        if key.code == KeyCode::Enter {
                            let sel = self.selected_row();
                            if let Some(Row::Dir { path, open, .. }) = sel {
                                let (path, open) = (path.clone(), *open);
                                self.fold(&path, !open);
                                return;
                            }
                            if let Some(path) = self.selected_path() {
                                self.ask(Then::Open(path));
                            }
                        }
                        let keep = self.selected_key();
                        self.refilter(keep.as_deref());
                        return;
                    }
                    _ => {
                        if !edit(&mut self.filter, &key) {
                            return;
                        }
                        self.prompt = Some(Prompt::Filter);
                    }
                }
                self.filter_changed();
            }
            Prompt::Search(mut s) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if !s.is_empty() {
                        self.search = s;
                    }
                    self.find(false);
                }
                _ => {
                    edit(&mut s, &key);
                    self.prompt = Some(Prompt::Search(s));
                }
            },
            Prompt::Goto(mut s) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match (s.trim().parse::<usize>(), self.file.as_mut()) {
                    (Ok(n), Some(f)) => f.buf.goto(buffer::Pos {
                        row: n.saturating_sub(1).min(f.buf.len().saturating_sub(1)),
                        col: 0,
                    }),
                    _ => self.set_flash("Escribe el número de una línea"),
                },
                // Solo cifras: una letra no se escribe.
                KeyCode::Char(c)
                    if !c.is_ascii_digit() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.prompt = Some(Prompt::Goto(s));
                }
                _ => {
                    edit(&mut s, &key);
                    self.prompt = Some(Prompt::Goto(s));
                }
            },
            Prompt::Discard(then) => match key.code {
                KeyCode::Char('y') => self.then(then),
                KeyCode::Char('s') => {
                    self.after_save = self.file.as_ref().map(|f| (f.gen, then));
                    self.save();
                }
                _ => {}
            },
        }
    }

    fn on_list(&mut self, key: KeyEvent) {
        let page = self.page.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_list(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_list(1),
            KeyCode::PageUp => self.move_list(-page),
            KeyCode::PageDown => self.move_list(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_list(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_list(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let sel = self.selected_row();
                match sel {
                    Some(Row::Dir { path, open, .. }) => {
                        let (path, open) = (path.clone(), *open);
                        if !open || key.code == KeyCode::Enter {
                            self.fold(&path, !open);
                        } else {
                            // Ya abierta: → baja a lo primero de adentro.
                            self.move_list(1);
                        }
                    }
                    Some(Row::File { .. }) => {
                        if let Some(path) = self.selected_path() {
                            self.ask(Then::Open(path));
                        }
                    }
                    None => {}
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let sel = self.selected_row();
                match sel {
                    Some(Row::Dir {
                        path, open: true, ..
                    }) => {
                        let path = path.clone();
                        self.fold(&path, false);
                    }
                    _ => self.select_parent(),
                }
            }
            KeyCode::Char('/') => self.prompt = Some(Prompt::Filter),
            KeyCode::Char('c') => {
                self.only_changed = !self.only_changed;
                if self.only_changed {
                    // Las carpetas con cambios, abiertas.
                    self.expanded_before = Some(self.expanded.clone());
                    self.open_changed();
                } else if let Some(before) = self.expanded_before.take() {
                    // Las de antes (y las de lo seleccionado, en `refilter`).
                    self.expanded = before;
                }
                let keep = self.selected_key();
                self.refilter(keep.as_deref());
            }
            KeyCode::Char('r') => {
                self.load_list();
                self.set_flash("Recargando la lista…");
            }
            KeyCode::Tab if self.file.is_some() => self.focus = Focus::File,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                let keep = self.selected_key();
                self.refilter(keep.as_deref());
            }
            KeyCode::Esc | KeyCode::Char('q') => self.ask(Then::Close),
            KeyCode::Char('<') => self.list_width = self.list_width.saturating_sub(4).max(20),
            KeyCode::Char('>') => self.list_width = (self.list_width + 4).min(120),
            _ => {}
        }
    }

    fn on_view(&mut self, key: KeyEvent) {
        let page = self.page.max(1) as isize;
        let Some(f) = self.file.as_mut() else {
            self.focus = Focus::List;
            return;
        };
        let b = &mut f.buf;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => b.vertical(-1),
            KeyCode::Down | KeyCode::Char('j') => b.vertical(1),
            KeyCode::Left | KeyCode::Char('h') => b.left(),
            KeyCode::Right | KeyCode::Char('l') => b.right(),
            KeyCode::PageUp => b.vertical(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => b.vertical(page),
            KeyCode::Home | KeyCode::Char('0') => b.home(),
            KeyCode::End | KeyCode::Char('$') => b.end(),
            KeyCode::Char('g') => b.goto(buffer::Pos::default()),
            KeyCode::Char('G') => b.goto(buffer::Pos {
                row: b.len().saturating_sub(1),
                col: 0,
            }),
            KeyCode::Char('i' | 'e') => f.editing = true,
            KeyCode::Char('/') => self.prompt = Some(Prompt::Search(String::new())),
            KeyCode::Char('n') if !self.search.is_empty() => self.find(false),
            KeyCode::Char('N') if !self.search.is_empty() => self.find(true),
            KeyCode::Char(':') => self.prompt = Some(Prompt::Goto(String::new())),
            KeyCode::Char('R') => self.ask(Then::Reload),
            KeyCode::Esc | KeyCode::Tab => self.focus = Focus::List,
            KeyCode::Char('q') => self.ask(Then::Close),
            _ => {}
        }
    }

    fn on_edit(&mut self, key: KeyEvent) {
        let page = self.page.max(1) as isize;
        let Some(f) = self.file.as_mut() else {
            return;
        };
        let b = &mut f.buf;
        let row = match key.code {
            KeyCode::Esc => {
                f.editing = false;
                b.seal();
                None
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                Some(b.insert(c.encode_utf8(&mut [0; 4])))
            }
            KeyCode::Enter => Some(b.newline()),
            KeyCode::Tab => {
                let unit = b.indent_unit();
                Some(b.insert(&unit))
            }
            KeyCode::Backspace => b.backspace(),
            KeyCode::Delete => b.delete(),
            KeyCode::Up => {
                b.vertical(-1);
                None
            }
            KeyCode::Down => {
                b.vertical(1);
                None
            }
            KeyCode::Left => {
                b.left();
                None
            }
            KeyCode::Right => {
                b.right();
                None
            }
            KeyCode::PageUp => {
                b.vertical(-page);
                None
            }
            KeyCode::PageDown => {
                b.vertical(page);
                None
            }
            KeyCode::Home => {
                b.home();
                None
            }
            KeyCode::End => {
                b.end();
                None
            }
            _ => None,
        };
        if let Some(row) = row {
            changed(f, row);
        }
    }

    // ---------- dibujo ----------

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        // En una terminal angosta, solo el panel con el foco.
        let narrow = body.width < 90;
        if narrow {
            match (self.focus, self.file.is_some() || self.opening.is_some()) {
                (Focus::File, true) => self.draw_file(f, body),
                _ => self.draw_list(f, body),
            }
        } else {
            let w = self.list_width.min(body.width / 2);
            let [left, right] =
                Layout::horizontal([Constraint::Length(w), Constraint::Min(0)]).areas(body);
            self.draw_list(f, left);
            self.draw_file(f, right);
        }
        self.draw_footer(f, footer);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let ic = icons::get();
        let focused = self.focus == Focus::List && self.prompt.is_none()
            || matches!(self.prompt, Some(Prompt::Filter));
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
            .title(format!(" {}{} ", pad(ic.folder), self.title));
        if let Some(l) = &self.listing {
            let changed = l.files.iter().filter(|e| e.change.is_some()).count();
            let mut info = format!(" {}/{} ", self.matches, l.files.len());
            if changed > 0 {
                info.push_str(&format!("· {changed} cambiados "));
            }
            if self.only_changed {
                info.push_str("· solo cambiados ");
            }
            block = block.title_bottom(info);
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        let filtering = matches!(self.prompt, Some(Prompt::Filter));
        let [filter, rest] = Layout::vertical([
            Constraint::Length(u16::from(filtering || !self.filter.is_empty())),
            Constraint::Min(0),
        ])
        .areas(inner);
        if filter.height > 0 {
            let line = Line::from(vec![
                Span::styled("/ ", Style::default().fg(ACCENT)),
                Span::raw(self.filter.clone()),
            ]);
            f.render_widget(line, filter);
            if filtering {
                let x = filter.x + 2 + text::width(&self.filter) as u16;
                f.set_cursor_position(Position::new(
                    x.min(filter.right().saturating_sub(1)),
                    filter.y,
                ));
            }
        }
        let Some(l) = &self.listing else {
            let msg = match &self.list_error {
                Some(e) => format!("No pude leer los archivos:\n{e}"),
                None => "Leyendo los archivos…".to_string(),
            };
            f.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                rest,
            );
            return;
        };
        if self.shown.is_empty() {
            let msg = if l.files.is_empty() {
                "No hay archivos"
            } else if self.only_changed && self.filter.is_empty() {
                "Nada cambió todavía (c: ver todos)"
            } else {
                "Ningún archivo coincide"
            };
            f.render_widget(
                Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
                rest,
            );
            return;
        }
        let open = self.file.as_ref().map(|f| f.path.as_str());
        let flat = !self.filter.is_empty();
        let w = (rest.width as usize).saturating_sub(2);
        let items: Vec<ListItem> = self
            .shown
            .iter()
            .map(|row| match row {
                Row::Dir {
                    path,
                    depth,
                    open,
                    changed,
                } => {
                    let name = base_name(path);
                    let head = format!(
                        "{}{}{}",
                        "  ".repeat(*depth),
                        if *open { "▾ " } else { "▸ " },
                        pad(ic.folder)
                    );
                    let room = w.saturating_sub(text::width(&head));
                    let mark = if *changed > 0 { "• " } else { "  " };
                    ListItem::new(Line::from(vec![
                        Span::styled(mark, Style::default().fg(Color::Yellow)),
                        Span::styled(head, Style::default().fg(Color::Blue)),
                        Span::styled(
                            truncate_left(&format!("{name}/"), room),
                            Style::default()
                                .fg(Color::Blue)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]))
                }
                Row::File { i, depth } => {
                    let e = &l.files[*i];
                    let (mark, color) = match e.change {
                        Some(Change::Modified) => ("M ", Color::Yellow),
                        Some(Change::Added) => ("+ ", Color::Green),
                        None => ("  ", Color::DarkGray),
                    };
                    let mut style = Style::default();
                    if Some(e.path.as_str()) == open {
                        style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                    } else if e.change.is_some() {
                        style = style.fg(color);
                    }
                    // En el árbol, el nombre a la altura del de las carpetas
                    // hermanas (después de la flecha y el icono).
                    let (indent, name) = if flat {
                        (String::new(), e.path.as_str())
                    } else {
                        let name = base_name(&e.path);
                        let skip = 2 * depth + 2 + text::width(&pad(ic.folder));
                        (" ".repeat(skip), name)
                    };
                    let room = w.saturating_sub(indent.len());
                    ListItem::new(Line::from(vec![
                        Span::styled(mark, Style::default().fg(color)),
                        Span::raw(indent),
                        Span::styled(truncate_left(name, room), style),
                    ]))
                }
            })
            .collect();
        let list = List::new(items).highlight_style(Style::default().bg(SELECTED));
        f.render_stateful_widget(list, rest, &mut self.list_sel);
        if self.focus == Focus::List {
            self.page = rest.height as usize;
        }
    }

    fn draw_file(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::File && self.prompt.is_none();
        let Some(file) = self.file.as_mut() else {
            let msg = match &self.opening {
                Some(p) => format!("Abriendo {p}…"),
                None => "Elige un archivo de la lista y pulsa ⏎".to_string(),
            };
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray));
            f.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(Color::DarkGray))
                    .block(block),
                area,
            );
            return;
        };
        let border = match (focused, file.editing) {
            (_, true) => Color::Yellow,
            (true, false) => ACCENT,
            (false, false) => Color::DarkGray,
        };
        let mut title = vec![Span::styled(
            format!(" {} ", file.path),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if file.buf.dirty() {
            title.push(Span::styled("● ", Style::default().fg(Color::Yellow)));
        }
        if let Some(p) = &self.opening {
            title.push(Span::styled(
                format!("· abriendo {p}… "),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let b = &file.buf;
        let mut info = format!(
            " ln {}/{} · col {} ",
            b.cursor.row + 1,
            b.len().max(b.cursor.row + 1),
            b.cursor_col() + 1
        );
        if let Some(c) = &file.colors {
            info.push_str(&format!("· {} ", c.name()));
        }
        info.push_str(if file.editing {
            "· EDITANDO "
        } else {
            "· LEYENDO "
        });
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .title(Line::from(title))
            .title_bottom(Line::from(info).right_aligned());
        let inner = block.inner(area);
        f.render_widget(block, area);

        let height = inner.height as usize;
        // Con la lista al lado, su página es la suya.
        if self.focus == Focus::File {
            self.page = height.saturating_sub(1).max(1);
        }
        // La línea vacía tras el último salto no es una línea más, salvo
        // con el cursor en ella (al bajar del final o escribir ahí).
        let shown_last = file.buf.len().max(file.buf.cursor.row + 1);
        let gutter = shown_last.to_string().len() + 1;
        let width = (inner.width as usize).saturating_sub(gutter + 1);
        if height == 0 || width == 0 {
            return;
        }
        // Que el cursor quede a la vista.
        let (row, col) = (file.buf.cursor.row, file.buf.cursor_col());
        if row < file.top {
            file.top = row;
        } else if row >= file.top + height {
            file.top = row + 1 - height;
        }
        if col < file.left {
            file.left = col;
        } else if col >= file.left + width {
            file.left = col + 1 - width;
        }
        let lines = file.buf.lines();
        let end = (file.top + height).min(lines.len());
        let painted = file
            .colors
            .as_mut()
            .map(|c| c.paint(lines, file.top..end))
            .unwrap_or_default();
        for (i, r) in (file.top..end).enumerate() {
            if r >= shown_last {
                break;
            }
            let y = inner.y + i as u16;
            let current = r == row;
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            if current && focused {
                f.buffer_mut()
                    .set_style(row_area, Style::default().bg(SELECTED));
            }
            let num = Span::styled(
                format!("{:>w$} ", r + 1, w = gutter - 1),
                Style::default().fg(if current {
                    Color::White
                } else {
                    Color::DarkGray
                }),
            );
            f.render_widget(num, Rect::new(inner.x, y, gutter as u16, 1));
            let hits = buffer::matches(&lines[r], &self.search);
            let line = render_line(&lines[r], painted.get(i), &hits, file.left, width);
            f.render_widget(
                line,
                Rect::new(inner.x + gutter as u16 + 1, y, width as u16, 1),
            );
        }
        if focused {
            let x = inner.x + (gutter + 1 + col - file.left) as u16;
            let y = inner.y + (row - file.top) as u16;
            f.set_cursor_position(Position::new(x, y));
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let prompt = |label: &str, input: &str| {
            let line = Line::from(vec![
                Span::styled(format!(" {label}"), Style::default().fg(ACCENT)),
                Span::raw(input.to_string()),
                Span::styled(
                    "  ⏎ aceptar · Esc cancelar",
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            let x = area.x + 1 + text::width(label) as u16 + text::width(input) as u16;
            (line, Some(x))
        };
        let (line, cursor) = match &self.prompt {
            Some(Prompt::Search(s)) => prompt("Buscar: ", s),
            Some(Prompt::Goto(s)) => prompt("Ir a la línea: ", s),
            Some(Prompt::Discard(then)) => {
                let path = self.file.as_ref().map_or("", |f| f.path.as_str());
                let what = match then {
                    Then::Close => "Salir",
                    Then::Open(_) => "Abrir otro",
                    Then::Reload => "Volver a leerlo",
                };
                let line = Line::from(vec![
                    Span::styled(
                        format!(" {path} tiene cambios sin guardar. "),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(format!(
                        "s guardar · y {} sin guardar · otra tecla cancela",
                        what.to_lowercase()
                    )),
                ]);
                (line, None)
            }
            _ => match &self.flash {
                Some((msg, _)) => (
                    Line::from(Span::styled(
                        format!(" {}{msg}", pad(icons::get().bell)),
                        Style::default().fg(Color::Yellow),
                    )),
                    None,
                ),
                None => {
                    let help = fit_help(self.help(), area.width.saturating_sub(1) as usize);
                    (
                        Line::from(Span::styled(
                            format!(" {help}"),
                            Style::default().fg(Color::DarkGray),
                        )),
                        None,
                    )
                }
            },
        };
        f.render_widget(line, area);
        if let Some(x) = cursor {
            f.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
        }
    }

    fn help(&self) -> &'static [(&'static str, u8)] {
        match (
            &self.prompt,
            self.focus,
            self.file.as_ref().map(|f| f.editing),
        ) {
            (Some(Prompt::Filter), ..) => &[
                ("escribe para filtrar", 0),
                ("⏎ abrir", 0),
                ("↑↓ aceptar", 1),
                ("Esc borrar filtro", 0),
            ],
            (_, Focus::File, Some(true)) => &[
                ("Esc dejar de editar", 0),
                ("Ctrl-s guardar", 0),
                ("Ctrl-z deshacer", 1),
                ("Ctrl-y rehacer", 2),
                ("Tab sangría", 3),
            ],
            (_, Focus::File, Some(false)) => &[
                ("↑↓←→ mover", 1),
                ("i editar", 0),
                ("/ buscar", 0),
                ("n N siguiente", 2),
                (": ir a línea", 2),
                ("g G inicio/fin", 3),
                ("Ctrl-s guardar", 1),
                ("R recargar", 3),
                ("Esc lista", 0),
                ("q salir", 0),
            ],
            // Filtrando no hay carpetas.
            _ if !self.filter.is_empty() => &[
                ("↑↓ mover", 1),
                ("⏎ abrir", 0),
                ("/ filtrar", 0),
                ("Esc borrar filtro", 0),
                ("c solo cambiados", 1),
                ("r recargar", 2),
                ("Tab al archivo", 2),
                ("< > ancho", 3),
                ("q salir", 0),
            ],
            _ => &[
                ("↑↓ mover", 1),
                ("⏎ abrir", 0),
                ("←→ carpetas", 1),
                ("/ filtrar", 0),
                ("c solo cambiados", 1),
                ("r recargar", 2),
                ("Tab al archivo", 2),
                ("< > ancho", 3),
                ("q salir", 0),
            ],
        }
    }
}

/// Cambió el texto desde la línea `row`: los colores de ahí para abajo se
/// vuelven a calcular.
fn changed(f: &mut FileView, row: usize) {
    if let Some(c) = f.colors.as_mut() {
        c.changed(row);
    }
}

/// El último tramo de una ruta con `/`.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Abre las carpetas que contienen `path`.
fn open_parents(expanded: &mut HashSet<String>, path: &str) {
    for (i, _) in path.match_indices('/') {
        expanded.insert(path[..i].to_string());
    }
}

/// La ruta de una fila: de la carpeta o del archivo.
fn row_path<'a>(files: &'a [Entry], row: &'a Row) -> &'a str {
    match row {
        Row::Dir { path, .. } => path,
        Row::File { i, .. } => &files[*i].path,
    }
}

/// Los archivos `picked` (índices de `files`) en árbol: en cada carpeta,
/// primero las carpetas y luego los archivos, por nombre. De las carpetas
/// cerradas no sale lo de adentro.
fn tree(files: &[Entry], picked: &[usize], expanded: &HashSet<String>) -> Vec<Row> {
    #[derive(Default)]
    struct Node<'a> {
        dirs: HashMap<&'a str, Node<'a>>,
        files: Vec<usize>,
        changed: usize,
    }

    fn walk(
        node: Node,
        prefix: &str,
        depth: usize,
        files: &[Entry],
        expanded: &HashSet<String>,
        rows: &mut Vec<Row>,
    ) {
        let mut dirs: Vec<_> = node.dirs.into_iter().collect();
        dirs.sort_by_cached_key(|(name, _)| name.to_lowercase());
        for (name, sub) in dirs {
            let path = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let open = expanded.contains(&path);
            rows.push(Row::Dir {
                path: path.clone(),
                depth,
                open,
                changed: sub.changed,
            });
            if open {
                walk(sub, &path, depth + 1, files, expanded, rows);
            }
        }
        let mut here = node.files;
        here.sort_by_cached_key(|&i| base_name(&files[i].path).to_lowercase());
        rows.extend(here.into_iter().map(|i| Row::File { i, depth }));
    }

    let mut root = Node::default();
    for &i in picked {
        let e = &files[i];
        let changed = usize::from(e.change.is_some());
        let dirs = e.path.rsplit_once('/').map_or("", |(d, _)| d);
        let mut node = &mut root;
        for d in dirs.split('/').filter(|d| !d.is_empty()) {
            node = node.dirs.entry(d).or_default();
            node.changed += changed;
        }
        node.files.push(i);
    }
    let mut rows = Vec::new();
    walk(root, "", 0, files, expanded, &mut rows);
    rows
}

/// Si `path` tiene las letras de `q` en orden (sin mayúsculas: `q` ya viene
/// en minúsculas). Lo que contiene `q` seguido va primero, y entre esos los
/// más cortos.
fn fuzzy(path: &str, q: &str) -> Option<(bool, usize)> {
    if q.is_empty() {
        return Some((false, 0));
    }
    let lower = path.to_lowercase();
    let mut rest = lower.as_str();
    for c in q.chars() {
        let i = rest.find(c)?;
        rest = &rest[i + c.len_utf8()..];
    }
    Some((!lower.contains(q), path.len()))
}

/// `s` en `w` columnas, cortando por la izquierda: de una ruta importa más
/// el final (el archivo).
fn truncate_left(s: &str, w: usize) -> String {
    if text::width(s) <= w {
        return s.to_string();
    }
    let mut used = 1;
    let mut start = s.len();
    for (i, g) in s.grapheme_indices(true).rev() {
        used += text::width(g);
        if used > w {
            break;
        }
        start = i;
    }
    format!("…{}", &s[start..])
}

/// Las columnas `left..left + width` de `line`, con sus colores y las
/// coincidencias de la búsqueda resaltadas.
fn render_line(
    line: &str,
    painted: Option<&Painted>,
    hits: &[std::ops::Range<usize>],
    left: usize,
    width: usize,
) -> Line<'static> {
    let (r, g, b) = syntax::plain();
    let base = Style::default().fg(Color::Rgb(r, g, b));
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = base;
    let mut push = |text: &str, style: Style, spans: &mut Vec<Span<'static>>| {
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push_str(text);
    };
    let mut col = 0;
    let mut seg = 0;
    for (i, g) in line.grapheme_indices(true) {
        if col >= left + width {
            break;
        }
        let w = buffer::grapheme_width(g, col);
        let mut style = base;
        if let Some(p) = painted {
            while seg < p.len() && p[seg].1.end <= i {
                seg += 1;
            }
            if let Some((paint, range)) = p.get(seg) {
                if range.contains(&i) {
                    let (r, g, b) = paint.rgb;
                    style = style.fg(Color::Rgb(r, g, b));
                    if paint.bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                }
            }
        }
        if hits.iter().any(|h| h.contains(&i)) {
            style = style.fg(Color::Black).bg(Color::Yellow);
        }
        let shown: String = if g == "\t" {
            " ".repeat(w)
        } else if g.chars().any(char::is_control) {
            "·".into()
        } else {
            g.to_string()
        };
        if col < left {
            // Un carácter ancho cortado por el borde: lo que se ve, en blanco.
            if col + w > left {
                push(&" ".repeat(col + w - left), style, &mut spans);
            }
        } else if col + w > left + width {
            break;
        } else {
            push(&shown, style, &mut spans);
        }
        col += w;
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

/// `pastor edit`: el editor solo, sin el resto de la interfaz.
pub fn run(source: Source, title: String, open: Option<String>) -> Result<()> {
    let mut term = ratatui::init();
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let mut ed = Editor::new(source, title, open);
    let result = (|| -> Result<()> {
        let mut dirty = true;
        while !ed.closed {
            if std::mem::take(&mut dirty) {
                term.draw(|f| ed.draw(f, f.area()))?;
            }
            if event::poll(Duration::from_millis(100))? {
                dirty = true;
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => ed.on_key(key),
                    Event::Paste(text) => ed.on_paste(&text),
                    _ => {}
                }
            }
            dirty |= ed.poll();
        }
        Ok(())
    })();
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn opened(path: &str, text: &str) -> Opened {
        Opened {
            path: path.into(),
            text: text.into(),
            hash: crate::files::hash(text.as_bytes()),
        }
    }

    #[test]
    fn abrir_otro_no_pisa_lo_que_escribiste_mientras() {
        let dir = std::env::temp_dir().join(format!("pastor-ed-{}", std::process::id()));
        let mut ed = Editor::new(Source::dir(dir), "x".into(), None);
        ed.file_gen = 1;
        ed.opened(1, opened("a.txt", "uno"));
        // Se pide b sin cambios en a; mientras llega, se escribe en a.
        ed.open("b.txt".into());
        let b = ed.file_gen;
        ed.on_key(key(KeyCode::Char('i')));
        ed.on_key(key(KeyCode::Char('x')));
        ed.opened(b, opened("b.txt", "dos"));
        assert_eq!(ed.file.as_ref().unwrap().path, "a.txt");
        assert!(ed.dirty());
        // Si al pedirlo ya se aceptó perderlos, sí se abre.
        ed.open("b.txt".into());
        let b = ed.file_gen;
        ed.opened(b, opened("b.txt", "dos"));
        assert_eq!(ed.file.as_ref().unwrap().path, "b.txt");
        // Un guardado de a ya no toca a b.
        ed.saved(1, "a.txt".into(), None, Ok("h".into()));
        assert_ne!(ed.file.as_ref().unwrap().hash, "h");
    }

    #[test]
    fn lo_que_sigues_escribiendo_tras_descartar_no_se_pierde() {
        let dir = std::env::temp_dir().join(format!("pastor-ed2-{}", std::process::id()));
        let mut ed = Editor::new(Source::dir(dir), "x".into(), None);
        ed.file_gen = 1;
        ed.opened(1, opened("a.txt", "uno"));
        ed.on_key(key(KeyCode::Char('i')));
        ed.on_key(key(KeyCode::Char('x')));
        // Con cambios, se acepta perderlos y, mientras llega b, se sigue
        // escribiendo la misma palabra: eso ya no se aceptó perder.
        ed.open("b.txt".into());
        let b = ed.file_gen;
        ed.on_key(key(KeyCode::Char('y')));
        ed.opened(b, opened("b.txt", "dos"));
        assert_eq!(ed.file.as_ref().unwrap().path, "a.txt");
    }

    fn entry(path: &str, change: Option<Change>) -> Entry {
        Entry {
            path: path.into(),
            change,
        }
    }

    fn rows(files: &[Entry], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Dir {
                    path,
                    depth,
                    open,
                    changed,
                } => format!("{depth} {path}/ {open} {changed}"),
                Row::File { i, depth } => format!("{depth} {}", files[*i].path),
            })
            .collect()
    }

    #[test]
    fn ordena_en_carpetas_primero_las_carpetas() {
        let files = vec![
            entry("README.md", None),
            entry("a.txt", None),
            entry("src/main.rs", Some(Change::Modified)),
            entry("src/ui/app.js", Some(Change::Added)),
            entry("Docs/x.md", None),
        ];
        let all: Vec<usize> = (0..files.len()).collect();
        let none = HashSet::new();
        assert_eq!(
            rows(&files, &tree(&files, &all, &none)),
            [
                "0 Docs/ false 0",
                "0 src/ false 2",
                "0 a.txt",
                "0 README.md"
            ]
        );
        let mut open = HashSet::new();
        open_parents(&mut open, "src/ui/app.js");
        assert_eq!(
            rows(&files, &tree(&files, &all, &open)),
            [
                "0 Docs/ false 0",
                "0 src/ true 2",
                "1 src/ui/ true 1",
                "2 src/ui/app.js",
                "1 src/main.rs",
                "0 a.txt",
                "0 README.md"
            ]
        );
    }

    #[test]
    fn las_flechas_abren_y_cierran_carpetas() {
        let dir = std::env::temp_dir().join(format!("pastor-ed3-{}", std::process::id()));
        let mut ed = Editor::new(Source::dir(dir), "x".into(), None);
        ed.listing = Some(Listing {
            files: vec![entry("src/a.rs", None), entry("z.txt", None)],
            ..Default::default()
        });
        ed.refilter(None);
        assert_eq!(ed.shown.len(), 2);
        assert_eq!(ed.selected_key().as_deref(), Some("src"));
        ed.on_key(key(KeyCode::Right));
        assert_eq!(ed.shown.len(), 3);
        ed.on_key(key(KeyCode::Right));
        assert_eq!(ed.selected_path().as_deref(), Some("src/a.rs"));
        ed.on_key(key(KeyCode::Left));
        assert_eq!(ed.selected_key().as_deref(), Some("src"));
        ed.on_key(key(KeyCode::Left));
        assert_eq!(ed.shown.len(), 2);
        // Filtrando, la ruta entera; al borrar el filtro, de vuelta al árbol
        // con el archivo a la vista.
        ed.on_key(key(KeyCode::Char('/')));
        ed.on_key(key(KeyCode::Char('a')));
        assert_eq!(ed.shown, [Row::File { i: 0, depth: 0 }]);
        ed.on_key(key(KeyCode::Esc));
        assert_eq!(ed.selected_path().as_deref(), Some("src/a.rs"));
        // Con `c` se abren las carpetas con cambios; al quitarlo, se vuelve
        // a como estaban.
        ed.on_key(key(KeyCode::Left));
        ed.on_key(key(KeyCode::Left));
        ed.listing.as_mut().unwrap().files[0].change = Some(Change::Modified);
        ed.on_key(key(KeyCode::Char('c')));
        assert_eq!(ed.shown.len(), 2);
        ed.on_key(key(KeyCode::Up));
        ed.on_key(key(KeyCode::Char('c')));
        assert_eq!(ed.selected_key().as_deref(), Some("src"));
        assert_eq!(ed.shown.len(), 2);
    }

    #[test]
    fn con_c_lo_que_cambia_al_recargar_queda_a_la_vista() {
        let dir = std::env::temp_dir().join(format!("pastor-ed4-{}", std::process::id()));
        let mut ed = Editor::new(Source::dir(dir), "x".into(), None);
        let listing = |change| Listing {
            files: vec![
                entry("a/x.rs", Some(Change::Modified)),
                entry("b/y.rs", change),
            ],
            ..Default::default()
        };
        ed.listing = Some(listing(None));
        ed.on_key(key(KeyCode::Char('c')));
        assert_eq!(ed.shown.len(), 2);
        ed.list_gen += 1;
        ed.tx
            .send(Loaded::List(ed.list_gen, Ok(listing(Some(Change::Added)))))
            .unwrap();
        ed.poll();
        assert_eq!(ed.shown.len(), 4);
    }

    #[test]
    fn filtra_por_letras_en_orden() {
        assert_eq!(fuzzy("src/app.js", ""), Some((false, 0)));
        assert!(fuzzy("src/app.js", "app").is_some_and(|(far, _)| !far));
        assert!(fuzzy("src/app.js", "sajs").is_some_and(|(far, _)| far));
        assert_eq!(fuzzy("src/app.js", "xyz"), None);
        assert_eq!(fuzzy("README.md", "readme"), Some((false, 9)));
    }

    #[test]
    fn corta_las_rutas_por_la_izquierda() {
        assert_eq!(truncate_left("src/app.js", 20), "src/app.js");
        assert_eq!(
            truncate_left("src/components/Boton.tsx", 12),
            "…s/Boton.tsx"
        );
        assert_eq!(text::width(&truncate_left("ñandú/🦀/archivo.rs", 10)), 10);
    }

    fn plain_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn dibuja_las_columnas_que_se_ven() {
        let l = render_line("\tab🦀cd", None, &[], 0, 20);
        assert_eq!(plain_text(&l), "    ab🦀cd");
        // Desde la columna 5: "b🦀cd".
        assert_eq!(
            plain_text(&render_line("\tab🦀cd", None, &[], 5, 20)),
            "b🦀cd"
        );
        // Media 🦀 cortada a la izquierda se ve como espacio, y a la
        // derecha no se dibuja.
        assert_eq!(plain_text(&render_line("ab🦀cd", None, &[], 3, 20)), " cd");
        assert_eq!(plain_text(&render_line("ab🦀cd", None, &[], 0, 3)), "ab");
        assert_eq!(plain_text(&render_line("a\x1bb", None, &[], 0, 9)), "a·b");
        #[allow(clippy::single_range_in_vec_init)]
        let hits = vec![1..4];
        let hit = render_line("xfoo", None, &hits, 0, 9);
        assert_eq!(hit.spans.len(), 2);
        assert_eq!(hit.spans[1].style.bg, Some(Color::Yellow));
    }
}
