//! Medidas de texto en columnas de terminal, no en caracteres: un emoji o un
//! carácter chino ocupan dos columnas, y un acento combinado ninguna.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Columnas que ocupa `s` en pantalla.
pub fn width(s: &str) -> usize {
    s.width()
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Rellena `s` con espacios hasta `w` columnas (no corta si ya es más ancho).
pub fn pad_right(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

/// `s` recortado a `n` columnas, con `…` al final si no cabía.
pub fn truncate(s: &str, n: usize) -> String {
    if width(s) <= n {
        return s.to_string();
    }
    let room = n.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > room {
            break;
        }
        used += cw;
        out.push(c);
    }
    out + "…"
}

/// Cuántos caracteres del principio de `chars` caben en `w` columnas. Al
/// menos uno, para avanzar aunque el primero sea más ancho que `w`.
pub fn fit(chars: &[char], w: usize) -> usize {
    let mut used = 0;
    for (i, c) in chars.iter().enumerate() {
        used += char_width(*c);
        if used > w {
            return i.max(1);
        }
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuenta_columnas() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("🚀x"), 3);
    }

    #[test]
    fn recorta_por_columnas() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // "日本" ya ocupa 4: con 4 columnas solo cabe "日" + "…".
        assert_eq!(truncate("日本語", 4), "日…");
        assert!(width(&truncate("日本語です", 5)) <= 5);
    }

    #[test]
    fn rellena_por_columnas() {
        assert_eq!(pad_right("日", 4), "日  ");
        assert_eq!(pad_right("abcde", 3), "abcde");
    }

    #[test]
    fn cabe_por_columnas() {
        let c: Vec<char> = "日本語".chars().collect();
        assert_eq!(fit(&c, 5), 2);
        assert_eq!(fit(&c, 6), 3);
        assert_eq!(fit(&c, 1), 1);
    }
}
