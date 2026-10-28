//! Colores de sintaxis para el editor, con las gramáticas y temas que trae
//! syntect. Se cargan una sola vez, la primera vez que se abre un archivo.

use std::ops::Range;
use std::sync::OnceLock;

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Una línea más larga que esto (código minificado) va sin colores: el
/// análisis tardaría en cada cuadro.
const MAX_LINE: usize = 4000;
/// Un archivo más grande que esto va sin colores.
pub const MAX_BYTES: usize = 1024 * 1024;
/// Cada cuántas líneas se guarda el estado del análisis: para pintar una
/// línea hay que analizar desde la marca anterior.
const EVERY: usize = 64;

struct Loaded {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn loaded() -> &'static Loaded {
    static LOADED: OnceLock<Loaded> = OnceLock::new();
    LOADED.get_or_init(|| Loaded {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        theme: ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default(),
    })
}

/// Un color con negrita o no.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Paint {
    pub rgb: (u8, u8, u8),
    pub bold: bool,
}

/// Los trozos de una línea con su color, como rangos de bytes.
pub type Painted = Vec<(Paint, Range<usize>)>;

/// La gramática para `path`. syntect no trae todas: las que faltan se
/// parecen lo suficiente a alguna que sí (TypeScript a JavaScript, un
/// `.env` a un script de shell).
fn find(path: &str, first_line: &str) -> Option<&'static SyntaxReference> {
    let ss = &loaded().syntaxes;
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    let alias = match ext {
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" | "mts" | "cts" | "json5" => Some("js"),
        "vue" | "svelte" | "astro" => Some("html"),
        "toml" | "ini" | "cfg" | "conf" => Some("properties"),
        "jsonc" | "webmanifest" => Some("json"),
        _ if name.starts_with(".env") || name == "Dockerfile" || name == "Procfile" => Some("sh"),
        _ => None,
    };
    alias
        .and_then(|e| ss.find_syntax_by_extension(e))
        .or_else(|| ss.find_syntax_by_extension(ext))
        .or_else(|| ss.find_syntax_by_extension(name))
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
}

/// Pinta las líneas de un archivo a medida que se ven, guardando el estado
/// del análisis cada `EVERY` líneas para no empezar siempre desde arriba.
pub struct Colors {
    syntax: &'static SyntaxReference,
    highlighter: Highlighter<'static>,
    /// El estado al empezar la línea `i * EVERY`.
    marks: Vec<(ParseState, HighlightState)>,
}

impl Colors {
    /// `None` si no hay gramática para el archivo.
    pub fn new(path: &str, first_line: &str) -> Option<Colors> {
        let syntax = find(path, first_line)?;
        let highlighter = Highlighter::new(&loaded().theme);
        let start = (
            ParseState::new(syntax),
            HighlightState::new(&highlighter, ScopeStack::new()),
        );
        Some(Colors {
            syntax,
            highlighter,
            marks: vec![start],
        })
    }

    /// El nombre de la gramática, para mostrarlo.
    pub fn name(&self) -> &str {
        &self.syntax.name
    }

    /// Cambió la línea `row`: lo que se sabía de las siguientes ya no vale.
    pub fn changed(&mut self, row: usize) {
        self.marks.truncate(row / EVERY + 1);
    }

    /// Los colores de las líneas `rows` de `lines`.
    pub fn paint<S: AsRef<str>>(&mut self, lines: &[S], rows: Range<usize>) -> Vec<Painted> {
        let end = rows.end.min(lines.len());
        let from = rows.start.min(end);
        let mark = (from / EVERY).min(self.marks.len() - 1);
        let (mut parse, mut hl) = self.marks[mark].clone();
        let mut out = Vec::with_capacity(end - from);
        for (row, line) in lines.iter().enumerate().take(end).skip(mark * EVERY) {
            if row % EVERY == 0 && row / EVERY == self.marks.len() {
                self.marks.push((parse.clone(), hl.clone()));
            }
            let painted = self.line(line.as_ref(), &mut parse, &mut hl);
            if row >= from {
                out.push(painted);
            }
        }
        out
    }

    fn line(&self, line: &str, parse: &mut ParseState, hl: &mut HighlightState) -> Painted {
        if line.len() > MAX_LINE {
            return Vec::new();
        }
        // La gramática espera el salto de línea al final.
        let with_nl = format!("{line}\n");
        let Ok(ops) = parse.parse_line(&with_nl, &loaded().syntaxes) else {
            return Vec::new();
        };
        let mut at = 0;
        let mut out = Vec::new();
        for (style, piece) in HighlightIterator::new(hl, &ops, &with_nl, &self.highlighter) {
            let end = (at + piece.len()).min(line.len());
            if end > at {
                let c = style.foreground;
                let paint = Paint {
                    rgb: (c.r, c.g, c.b),
                    bold: style.font_style.contains(FontStyle::BOLD),
                };
                out.push((paint, at..end));
            }
            at += piece.len();
        }
        out
    }
}

/// El color de lo que no tiene uno propio.
pub fn plain() -> (u8, u8, u8) {
    let c = loaded()
        .theme
        .settings
        .foreground
        .unwrap_or(syntect::highlighting::Color::WHITE);
    (c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elige_la_gramatica_por_nombre() {
        let name = |p: &str| Colors::new(p, "").map(|c| c.name().to_string());
        assert_eq!(name("src/app.js").as_deref(), Some("JavaScript"));
        assert_eq!(name("web/main.tsx").as_deref(), Some("JavaScript"));
        assert_eq!(name("src/main.rs").as_deref(), Some("Rust"));
        assert!(name(".env").unwrap().contains("bash"));
        assert!(name(".env.local").unwrap().contains("bash"));
        assert_eq!(name("notas.txt"), None);
        assert_eq!(name("sin-extension"), None);
        assert!(Colors::new("script", "#!/bin/bash").is_some());
    }

    #[test]
    fn pinta_igual_desde_cualquier_marca() {
        let lines: Vec<String> = (0..300)
            .map(|i| format!("let x{i} = \"texto {i}\"; // nota"))
            .collect();
        let mut c = Colors::new("a.js", "").unwrap();
        let all = c.paint(&lines, 0..300);
        assert_eq!(all.len(), 300);
        // Los rangos cubren la línea, sin pasarse.
        for (p, line) in all.iter().zip(&lines) {
            assert_eq!(p.last().unwrap().1.end, line.len());
        }
        let mut fresh = Colors::new("a.js", "").unwrap();
        assert_eq!(fresh.paint(&lines, 200..210), all[200..210].to_vec());
        c.changed(130);
        assert_eq!(c.paint(&lines, 250..260), all[250..260].to_vec());
        // Fuera del archivo, nada.
        assert!(c.paint(&lines, 400..410).is_empty());
    }
}
