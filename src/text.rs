//! Medidas de texto en columnas de terminal, no en caracteres: un emoji o un
//! carácter chino ocupan dos columnas, y un acento combinado ninguna.
//!
//! Se mide grafema a grafema, igual que ratatui al pintar, para que un emoji
//! compuesto (❤️, 👨‍👩‍👧, 🇪🇸) cuente lo mismo aquí que en pantalla y no se parta.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn graphemes(s: &str) -> impl Iterator<Item = (usize, &str)> {
    s.grapheme_indices(true)
}

/// Columnas que ocupa `s` en pantalla.
pub fn width(s: &str) -> usize {
    graphemes(s).map(|(_, g)| g.width()).sum()
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
    if n == 0 {
        return String::new();
    }
    let room = n - 1;
    let mut used = 0;
    let mut end = 0;
    for (i, g) in graphemes(s) {
        used += g.width();
        if used > room {
            break;
        }
        end = i + g.len();
    }
    format!("{}…", &s[..end])
}

/// Bytes del principio de `s` que caben en `w` columnas, sin partir un
/// grafema. Al menos uno, para avanzar aunque el primero sea más ancho que `w`.
pub fn fit(s: &str, w: usize) -> usize {
    let mut used = 0;
    for (i, g) in graphemes(s) {
        used += g.width();
        if used > w {
            return if i == 0 { g.len() } else { i };
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuenta_columnas() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("🚀x"), 3);
        assert_eq!(width("❤️❤️"), 4);
    }

    #[test]
    fn recorta_por_columnas() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // "日本" ya ocupa 4: con 4 columnas solo cabe "日" + "…".
        assert_eq!(truncate("日本語", 4), "日…");
        assert!(width(&truncate("日本語です", 5)) <= 5);
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn recorta_sin_partir_emoji() {
        assert_eq!(truncate("❤️❤️❤️", 4), "❤️…");
        assert_eq!(truncate("👨‍👩‍👧 equipo", 3), "👨‍👩‍👧…");
        assert_eq!(truncate("🇪🇸🇲🇽x", 3), "🇪🇸…");
    }

    #[test]
    fn rellena_por_columnas() {
        assert_eq!(pad_right("日", 4), "日  ");
        assert_eq!(pad_right("abcde", 3), "abcde");
    }

    #[test]
    fn cabe_por_columnas() {
        assert_eq!(fit("日本語", 5), "日本".len());
        assert_eq!(fit("日本語", 6), "日本語".len());
        assert_eq!(fit("日本語", 1), "日".len());
        assert_eq!(fit("✔️✔️✔️", 4), "✔️✔️".len());
    }
}
