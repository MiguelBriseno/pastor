//! El texto de un archivo abierto en el editor: sus líneas, el cursor y lo
//! que se puede deshacer. Las columnas son bytes dentro de la línea, siempre
//! al borde de un grafema; en pantalla se miden en columnas de terminal, con
//! los tabuladores cada `TAB` columnas.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const TAB: usize = 4;
/// Cuántos cambios se pueden deshacer.
const UNDO: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

/// Un cambio: en `at` se quitó `removed` y se puso `inserted`.
#[derive(Clone, Debug)]
struct Edit {
    id: u64,
    at: Pos,
    removed: String,
    inserted: String,
    /// Dónde estaba el cursor antes, para volver ahí al deshacer.
    before: Pos,
}

pub struct Buffer {
    lines: Vec<String>,
    /// Si el archivo usaba `\r\n`: se guarda igual.
    crlf: bool,
    pub cursor: Pos,
    /// La columna en pantalla a la que vuelve el cursor al subir y bajar
    /// por líneas más cortas.
    goal: Option<usize>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_id: u64,
    /// El cambio de arriba de `undo` cuando se guardó (o se abrió).
    saved: Option<u64>,
    /// Si el siguiente cambio puede sumarse al anterior (seguir escribiendo
    /// la misma palabra se deshace de una vez).
    joinable: bool,
}

/// Ancho en pantalla de un grafema que empieza en la columna `col`.
/// Los de control (salvo el tabulador) se ven como un `·`: escritos tal cual
/// moverían el cursor de la terminal.
pub fn grapheme_width(g: &str, col: usize) -> usize {
    if g == "\t" {
        TAB - col % TAB
    } else if g.chars().any(char::is_control) {
        1
    } else {
        g.width()
    }
}

/// Columna en pantalla del byte `byte` de `line`.
pub fn display_col(line: &str, byte: usize) -> usize {
    let mut col = 0;
    for (i, g) in line.grapheme_indices(true) {
        if i >= byte {
            break;
        }
        col += grapheme_width(g, col);
    }
    col
}

/// El byte de `line` que queda en la columna `want` de la pantalla (o el
/// último antes de ella).
fn byte_at(line: &str, want: usize) -> usize {
    let mut col = 0;
    for (i, g) in line.grapheme_indices(true) {
        let w = grapheme_width(g, col);
        if col + w > want {
            return i;
        }
        col += w;
    }
    line.len()
}

fn prev_boundary(line: &str, col: usize) -> usize {
    line[..col]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

fn next_boundary(line: &str, col: usize) -> usize {
    line[col..]
        .graphemes(true)
        .next()
        .map_or(col, |g| col + g.len())
}

/// Dónde termina `text` si se pone en `at`.
fn end_of(at: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        None => Pos {
            row: at.row,
            col: at.col + text.len(),
        },
        Some(i) => Pos {
            row: at.row + text.matches('\n').count(),
            col: text.len() - i - 1,
        },
    }
}

impl Buffer {
    pub fn new(text: &str) -> Buffer {
        // Solo si todas las líneas acaban en `\r\n`: con finales mezclados,
        // los `\r` se quedan en sus líneas y se guarda tal cual.
        let crlf =
            text.contains("\r\n") && text.matches('\n').count() == text.matches("\r\n").count();
        let text = if crlf {
            text.replace("\r\n", "\n")
        } else {
            text.to_string()
        };
        Buffer {
            lines: text.split('\n').map(String::from).collect(),
            crlf,
            cursor: Pos::default(),
            goal: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 0,
            saved: None,
            joinable: false,
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line(&self, row: usize) -> &str {
        self.lines.get(row).map_or("", String::as_str)
    }

    /// Cuántas líneas hay. Un archivo que termina en salto de línea no
    /// cuenta la vacía de después.
    pub fn len(&self) -> usize {
        match self.lines.last() {
            Some(l) if l.is_empty() && self.lines.len() > 1 => self.lines.len() - 1,
            _ => self.lines.len(),
        }
    }

    pub fn text(&self) -> String {
        self.lines.join(if self.crlf { "\r\n" } else { "\n" })
    }

    pub fn dirty(&self) -> bool {
        self.undo.last().map(|e| e.id) != self.saved
    }

    /// El punto que se va a guardar: lo que se escriba mientras se guarda
    /// (por SSH tarda) queda como cambio aparte, sin guardar.
    /// El último cambio: si es otro, el texto cambió (o se deshizo algo).
    pub fn version(&self) -> Option<u64> {
        self.undo.last().map(|e| e.id)
    }

    pub fn checkpoint(&mut self) -> Option<u64> {
        self.joinable = false;
        self.undo.last().map(|e| e.id)
    }

    /// Ya se guardó lo que había en `checkpoint`.
    pub fn saved_as(&mut self, at: Option<u64>) {
        self.saved = at;
    }

    #[cfg(test)]
    fn mark_saved(&mut self) {
        let at = self.checkpoint();
        self.saved_as(at);
    }

    /// La sangría que usa el archivo: tabuladores si alguna línea empieza
    /// con uno; si no, `TAB` espacios.
    pub fn indent_unit(&self) -> String {
        if self.lines.iter().any(|l| l.starts_with('\t')) {
            "\t".into()
        } else {
            " ".repeat(TAB)
        }
    }

    /// La columna en pantalla del cursor.
    pub fn cursor_col(&self) -> usize {
        display_col(self.line(self.cursor.row), self.cursor.col)
    }

    // ---------- cambios ----------

    /// Quita el texto entre `a` y `b` (con `a <= b`) y lo devuelve.
    fn take(&mut self, a: Pos, b: Pos) -> String {
        if a.row == b.row {
            return self.lines[a.row].drain(a.col..b.col).collect();
        }
        let tail = self.lines[b.row][b.col..].to_string();
        let mut removed = self.lines[a.row].split_off(a.col);
        for l in self.lines.drain(a.row + 1..=b.row) {
            removed.push('\n');
            removed.push_str(&l);
        }
        // Lo de la última línea que sí se queda no es parte de lo quitado.
        removed.truncate(removed.len() - tail.len());
        self.lines[a.row].push_str(&tail);
        removed
    }

    /// Pone `text` en `at`. Devuelve dónde termina.
    fn put(&mut self, at: Pos, text: &str) -> Pos {
        let tail = self.lines[at.row].split_off(at.col);
        let mut parts = text.split('\n');
        self.lines[at.row].push_str(parts.next().unwrap_or(""));
        let mut row = at.row;
        for p in parts {
            row += 1;
            self.lines.insert(row, p.to_string());
        }
        let end = Pos {
            row,
            col: self.lines[row].len(),
        };
        self.lines[row].push_str(&tail);
        end
    }

    /// Cambia el texto entre `a` y `b` por `text`, deja el cursor al final y
    /// lo anota para deshacerlo. Devuelve la primera línea que cambió.
    fn edit(&mut self, a: Pos, b: Pos, text: &str) -> usize {
        let before = self.cursor;
        let removed = self.take(a, b);
        self.cursor = self.put(a, text);
        self.goal = None;
        self.redo.clear();
        let new = Edit {
            id: self.next_id,
            at: a,
            removed,
            inserted: text.to_string(),
            before,
        };
        if !(self.joinable && !text.contains('\n') && self.join(&new)) {
            self.next_id += 1;
            self.undo.push(new);
            if self.undo.len() > UNDO {
                let oldest = self.undo.remove(0);
                // Sin el más viejo, deshacerlo todo deja el texto de después
                // de él: si era lo guardado, ahí está; si lo guardado era de
                // antes, ya no se puede volver.
                self.saved = match self.saved {
                    None => Some(u64::MAX),
                    Some(id) if id == oldest.id => None,
                    s => s,
                };
            }
        }
        // Escribiendo una palabra se suma; un salto de línea la cierra.
        self.joinable = !text.contains('\n');
        a.row
    }

    /// Suma `new` al último cambio si lo continúa: seguir escribiendo,
    /// borrar hacia atrás o hacia adelante desde el mismo lugar.
    fn join(&mut self, new: &Edit) -> bool {
        let Some(last) = self.undo.last_mut() else {
            return false;
        };
        if self.saved == Some(last.id) {
            return false;
        }
        let typing = last.removed.is_empty() && new.removed.is_empty();
        let deleting = last.inserted.is_empty() && new.inserted.is_empty();
        if typing && new.at == end_of(last.at, &last.inserted) {
            last.inserted.push_str(&new.inserted);
        } else if deleting && end_of(new.at, &new.removed) == last.at {
            last.removed = format!("{}{}", new.removed, last.removed);
            last.at = new.at;
        } else if deleting && new.at == last.at {
            last.removed.push_str(&new.removed);
        } else {
            return false;
        }
        true
    }

    /// Escribe `text` en el cursor. Devuelve la primera línea que cambió.
    pub fn insert(&mut self, text: &str) -> usize {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.edit(self.cursor, self.cursor, &text)
    }

    /// Salto de línea con la misma sangría que la línea de arriba.
    pub fn newline(&mut self) -> usize {
        let line = self.line(self.cursor.row);
        let indent: String = line[..self.cursor.col]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        self.insert(&format!("\n{indent}"))
    }

    pub fn backspace(&mut self) -> Option<usize> {
        let c = self.cursor;
        let from = if c.col > 0 {
            Pos {
                row: c.row,
                col: prev_boundary(self.line(c.row), c.col),
            }
        } else if c.row > 0 {
            Pos {
                row: c.row - 1,
                col: self.lines[c.row - 1].len(),
            }
        } else {
            return None;
        };
        Some(self.edit(from, c, ""))
    }

    pub fn delete(&mut self) -> Option<usize> {
        let c = self.cursor;
        let line = self.line(c.row);
        let to = if c.col < line.len() {
            Pos {
                row: c.row,
                col: next_boundary(line, c.col),
            }
        } else if c.row + 1 < self.lines.len() {
            Pos {
                row: c.row + 1,
                col: 0,
            }
        } else {
            return None;
        };
        let row = self.edit(c, to, "");
        self.cursor = c;
        Some(row)
    }

    /// Deshace el último cambio. Devuelve la primera línea que cambió.
    pub fn undo(&mut self) -> Option<usize> {
        let e = self.undo.pop()?;
        let end = end_of(e.at, &e.inserted);
        self.take(e.at, end);
        self.put(e.at, &e.removed);
        self.cursor = e.before;
        self.after_history();
        let row = e.at.row;
        self.redo.push(e);
        Some(row)
    }

    pub fn redo(&mut self) -> Option<usize> {
        let e = self.redo.pop()?;
        let end = end_of(e.at, &e.removed);
        self.take(e.at, end);
        self.cursor = self.put(e.at, &e.inserted);
        self.after_history();
        let row = e.at.row;
        self.undo.push(e);
        Some(row)
    }

    fn after_history(&mut self) {
        self.goal = None;
        self.joinable = false;
        self.clamp();
    }

    // ---------- movimiento ----------

    /// Lo que se escriba después ya no se suma al cambio anterior.
    pub fn seal(&mut self) {
        self.joinable = false;
    }

    fn clamp(&mut self) {
        self.cursor.row = self.cursor.row.min(self.lines.len() - 1);
        let line = &self.lines[self.cursor.row];
        let mut col = self.cursor.col.min(line.len());
        while !line.is_char_boundary(col) {
            col -= 1;
        }
        self.cursor.col = col;
    }

    pub fn left(&mut self) {
        let c = self.cursor;
        if c.col > 0 {
            self.cursor.col = prev_boundary(self.line(c.row), c.col);
        } else if c.row > 0 {
            self.cursor = Pos {
                row: c.row - 1,
                col: self.lines[c.row - 1].len(),
            };
        }
        self.goal = None;
        self.seal();
    }

    pub fn right(&mut self) {
        let c = self.cursor;
        let line = self.line(c.row);
        if c.col < line.len() {
            self.cursor.col = next_boundary(line, c.col);
        } else if c.row + 1 < self.lines.len() {
            self.cursor = Pos {
                row: c.row + 1,
                col: 0,
            };
        }
        self.goal = None;
        self.seal();
    }

    /// Sube (negativo) o baja `n` líneas, en la misma columna de pantalla.
    pub fn vertical(&mut self, n: isize) {
        let goal = *self
            .goal
            .get_or_insert_with(|| display_col(&self.lines[self.cursor.row], self.cursor.col));
        let last = self
            .len()
            .saturating_sub(1)
            .max(self.cursor.row.min(self.lines.len() - 1));
        let row = self.cursor.row.saturating_add_signed(n).min(last);
        self.cursor = Pos {
            row,
            col: byte_at(&self.lines[row], goal),
        };
        self.seal();
    }

    pub fn home(&mut self) {
        // Primero al inicio del texto; si ya estaba ahí, a la columna 0.
        let line = self.line(self.cursor.row);
        let indent = line.len() - line.trim_start().len();
        self.cursor.col = if self.cursor.col == indent { 0 } else { indent };
        self.goal = None;
        self.seal();
    }

    pub fn end(&mut self) {
        self.cursor.col = self.line(self.cursor.row).len();
        self.goal = None;
        self.seal();
    }

    pub fn goto(&mut self, pos: Pos) {
        self.cursor = pos;
        self.clamp();
        self.goal = None;
        self.seal();
    }

    // ---------- búsqueda ----------

    /// La siguiente aparición de `query` después del cursor (o antes, con
    /// `back`), dando la vuelta al archivo.
    pub fn find(&self, query: &str, back: bool) -> Option<Pos> {
        if query.is_empty() {
            return None;
        }
        let n = self.lines.len();
        let c = self.cursor;
        for step in 0..=n {
            let row = if back {
                (c.row + n * 2 - step) % n
            } else {
                (c.row + step) % n
            };
            let found = matches(&self.lines[row], query);
            let hit = if back {
                found
                    .iter()
                    .rev()
                    .find(|r| step > 0 || r.start < c.col)
                    .or(if step == n { found.last() } else { None })
            } else {
                found
                    .iter()
                    .find(|r| step > 0 || r.start > c.col)
                    .or(if step == n { found.first() } else { None })
            };
            if let Some(r) = hit {
                return Some(Pos { row, col: r.start });
            }
        }
        None
    }
}

/// Dónde aparece `query` en `line`, sin distinguir mayúsculas si es ASCII.
pub fn matches(line: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    // En minúsculas solo ASCII: así los bytes siguen en su lugar.
    let (hay, needle) = if query.is_ascii() {
        (line.to_ascii_lowercase(), query.to_ascii_lowercase())
    } else {
        (line.to_string(), query.to_string())
    };
    hay.match_indices(&needle)
        .map(|(i, m)| i..i + m.len())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(b: &mut Buffer, s: &str) {
        for c in s.chars() {
            b.insert(&c.to_string());
        }
    }

    #[test]
    fn guarda_tal_cual_lo_que_abrio() {
        for text in [
            "",
            "a",
            "a\n",
            "a\nb",
            "a\r\nb\r\n",
            "a\r\nb\n",
            "\n\n",
            "ñ\t🦀\n",
        ] {
            let b = Buffer::new(text);
            assert_eq!(b.text(), text);
            assert!(!b.dirty());
        }
        assert_eq!(Buffer::new("a\nb\n").len(), 2);
        assert_eq!(Buffer::new("a\nb").len(), 2);
        assert_eq!(Buffer::new("").len(), 1);
    }

    #[test]
    fn escribir_y_deshacer_por_palabras() {
        let mut b = Buffer::new("x\n");
        typed(&mut b, "hola");
        b.newline();
        typed(&mut b, "mundo");
        assert_eq!(b.text(), "hola\nmundox\n");
        assert!(b.dirty());
        b.undo();
        assert_eq!(b.text(), "hola\nx\n");
        b.undo();
        assert_eq!(b.text(), "holax\n");
        b.undo();
        assert_eq!(b.text(), "x\n");
        assert_eq!(b.cursor, Pos::default());
        assert!(!b.dirty());
        assert_eq!(b.undo(), None);
        b.redo();
        b.redo();
        b.redo();
        assert_eq!(b.text(), "hola\nmundox\n");
        assert_eq!(b.cursor, Pos { row: 1, col: 5 });
    }

    #[test]
    fn borrar_une_lineas_y_se_deshace_junto() {
        let mut b = Buffer::new("ab\ncd");
        b.goto(Pos { row: 1, col: 1 });
        b.backspace();
        b.backspace();
        b.backspace();
        assert_eq!(b.text(), "ad");
        assert_eq!(b.cursor, Pos { row: 0, col: 1 });
        b.undo();
        assert_eq!(b.text(), "ab\ncd");
        b.goto(Pos { row: 0, col: 1 });
        b.delete();
        assert_eq!(b.text(), "a\ncd");
        b.delete();
        assert_eq!(b.text(), "acd");
        assert_eq!(b.cursor, Pos { row: 0, col: 1 });
        b.undo();
        assert_eq!(b.text(), "ab\ncd");
    }

    #[test]
    fn guardar_corta_el_cambio_en_curso() {
        let mut b = Buffer::new("");
        typed(&mut b, "ab");
        b.mark_saved();
        assert!(!b.dirty());
        typed(&mut b, "c");
        assert!(b.dirty());
        b.undo();
        assert_eq!(b.text(), "ab");
        assert!(!b.dirty());
    }

    #[test]
    fn pegar_varias_lineas_y_sangria() {
        let mut b = Buffer::new("    fn x() {");
        b.end();
        b.newline();
        typed(&mut b, "y");
        assert_eq!(b.text(), "    fn x() {\n    y");
        b.insert("1\r\n2\r3");
        assert_eq!(b.text(), "    fn x() {\n    y1\n2\n3");
        assert_eq!(b.cursor, Pos { row: 3, col: 1 });
        b.undo();
        assert_eq!(b.text(), "    fn x() {\n    y");
    }

    #[test]
    fn se_mueve_por_grafemas_y_columnas() {
        let mut b = Buffer::new("añ🦀b\n\tx\nlarguísima");
        b.right();
        b.right();
        assert_eq!(b.cursor.col, "añ".len());
        b.right();
        assert_eq!(b.cursor_col(), 4);
        b.vertical(1);
        // En la columna 4 de "\tx" está la x.
        assert_eq!(b.cursor, Pos { row: 1, col: 1 });
        b.vertical(1);
        assert_eq!(b.cursor_col(), 4);
        b.vertical(-2);
        assert_eq!(b.cursor_col(), 4);
        b.vertical(10);
        assert_eq!(b.cursor.row, 2);
        b.backspace();
        assert_eq!(b.line(2), "laruísima");
    }

    #[test]
    fn busca_hacia_adelante_y_atras() {
        let mut b = Buffer::new("Foo bar\nbaz foo\nfoo");
        assert_eq!(b.find("foo", false), Some(Pos { row: 1, col: 4 }));
        b.goto(Pos { row: 2, col: 0 });
        assert_eq!(b.find("FOO", false), Some(Pos { row: 0, col: 0 }));
        assert_eq!(b.find("foo", true), Some(Pos { row: 1, col: 4 }));
        b.goto(Pos { row: 0, col: 0 });
        assert_eq!(b.find("foo", true), Some(Pos { row: 2, col: 0 }));
        // Si solo está donde está el cursor, la encuentra al dar la vuelta.
        let b = Buffer::new("uno");
        assert_eq!(b.find("uno", false), Some(Pos { row: 0, col: 0 }));
        assert_eq!(b.find("nada", false), None);
        // Sin ASCII distingue mayúsculas: pasarlas a minúsculas movería los bytes.
        assert_eq!(matches("ñañaÑA", "ña"), vec![0..3, 3..6]);
        assert_eq!(matches("xFOOfoo", "foo"), vec![1..4, 4..7]);
    }
}
