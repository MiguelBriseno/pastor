//! Contestar un diálogo de permiso desde el aviso del celular: el aviso de
//! ntfy lleva botones «Aprobar» y «Rechazar» que llaman a `pastor serve`.
//!
//! Cada botón es una URL de un solo uso (`/act/<token>`), que caduca en una
//! hora y solo vale para el diálogo tal como se veía al avisar: al usarla, si
//! la pantalla del agente ya es otra (respondiste desde la compu, o salió
//! otro diálogo), no pulsa nada. Así un botón viejo nunca contesta algo que
//! no viste.
//!
//! «Aprobar» no es pulsar Enter: en el diálogo de confianza de Claude Code la
//! opción marcada es «No, exit». Se leen las opciones de la pantalla, se busca
//! la de sí (o la de no) y se llega a ella con las flechas.

use std::fs;
use std::path::Path;

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::hooks;
use crate::store::{self, data_dir, write_atomic_mode};

/// Cuánto vale un botón.
const TTL_MINUTES: i64 = 60;

/// Un diálogo con opciones, leído de la pantalla.
#[derive(Debug, PartialEq, Eq)]
pub struct Dialog {
    /// Lo que pregunta: las líneas de encima de las opciones.
    pub question: Vec<String>,
    /// Las opciones, sin la marca del cursor (`1. Yes`, `No, exit`…).
    pub options: Vec<String>,
    /// La que está marcada.
    pub cursor: usize,
}

/// Marcas del cursor de los menús: la de Claude Code y las de Codex.
const CURSORS: [char; 3] = ['❯', '›', '▶'];

/// Columna (en caracteres) del primer carácter que no es espacio.
fn indent(line: &str) -> Option<usize> {
    line.chars().position(|c| !c.is_whitespace())
}

/// Si es una raya continua o el borde de una caja: lo que separa el diálogo
/// de lo de arriba.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| "─━═│╭╮╰╯┃".contains(c))
}

/// Si es una raya punteada: Claude Code rodea con ellas el comando dentro
/// del diálogo.
fn is_dashed(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| "╌╍┄┅┈┉-".contains(c))
}

/// El último menú de la pantalla: la línea con el cursor y sus vecinas que
/// empiezan en la misma columna que su texto (las que van más adentro son la
/// continuación de una opción larga).
pub fn parse(screen: &str) -> Option<Dialog> {
    let lines: Vec<&str> = screen.lines().map(unbox_keep_indent).collect();
    let (cur, col) = lines.iter().enumerate().rev().find_map(|(i, l)| {
        let at = indent(l)?;
        let rest: String = l.chars().skip(at).collect();
        let mark = rest.chars().next()?;
        if !CURSORS.contains(&mark) {
            return None;
        }
        let after = rest.chars().skip(1).take_while(|c| *c == ' ').count();
        // El cursor del prompt vacío (`❯ `) no es un menú.
        (rest.chars().nth(1 + after).is_some()).then_some((i, at + 1 + after))
    })?;
    // Hacia arriba, el principio del menú.
    let mut first = cur;
    while first > 0 {
        let l = lines[first - 1];
        match indent(l) {
            Some(i) if i >= col => first -= 1,
            _ => break,
        }
    }
    // Las opciones: las que empiezan justo en `col` (o la del cursor).
    let mut options: Vec<String> = Vec::new();
    let mut cursor = None;
    let mut i = first;
    while i < lines.len() {
        let l = lines[i];
        let Some(at) = indent(l) else { break };
        let text: String = l.chars().skip(at).collect();
        if i == cur {
            cursor = Some(options.len());
            options.push(
                text.chars()
                    .skip(col - at)
                    .collect::<String>()
                    .trim()
                    .to_string(),
            );
        } else if at == col {
            options.push(text.trim().to_string());
        } else if at > col && !options.is_empty() {
            // La continuación de una opción larga.
            let last = options.last_mut()?;
            last.push(' ');
            last.push_str(text.trim());
        } else {
            break;
        }
        i += 1;
    }
    if options.len() < 2 {
        return None;
    }
    // La pregunta: las líneas con texto de encima, hasta una separación
    // (Claude Code pone una raya encima del diálogo) o 8 líneas.
    let mut question = Vec::new();
    for l in lines[..first].iter().rev() {
        if is_rule(l) {
            break;
        }
        if l.trim().is_empty() || is_dashed(l) {
            continue;
        }
        question.push(l.trim().to_string());
        if question.len() == 8 {
            break;
        }
    }
    question.reverse();
    Some(Dialog {
        question,
        options,
        cursor: cursor?,
    })
}

/// La línea sin los bordes de caja, pero con su sangría: el menú se ubica
/// por columnas.
fn unbox_keep_indent(line: &str) -> &str {
    let l = line.trim_end().trim_end_matches(['│', '┃']).trim_end();
    match l.trim_start().strip_prefix(['│', '┃']) {
        Some(rest) => rest,
        None => l,
    }
}

/// Una opción sin su número: `2. Yes, and…` → `Yes, and…`.
fn bare(option: &str) -> &str {
    let t = option.trim();
    match t.split_once(". ") {
        Some((n, rest)) if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => rest,
        _ => t,
    }
}

/// Si la primera palabra de la opción es una de `words`.
fn starts_with_word(option: &str, words: &[&str]) -> bool {
    let first: String = bare(option)
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    words.contains(&first.as_str())
}

/// Lo que convierte un sí en un sí con algo más.
const EXTRA: &[&str] = &[
    " and ",
    ", and",
    " y ",
    "always",
    "siempre",
    "don't ask",
    "no preguntes",
    "bypass",
    "auto",
    "session",
    "sesión",
];

impl Dialog {
    /// La opción de aprobar: la primera que dice sí y nada más. Un sí con
    /// algo más («Yes, and bypass permissions», «Yes, and don't ask again»)
    /// da permisos que no ves en el aviso: si solo hay de esos, no hay botón.
    pub fn approve(&self) -> Option<usize> {
        self.options.iter().position(|o| {
            let rest = bare(o).to_lowercase();
            starts_with_word(o, &["yes", "sí", "si", "allow", "approve", "proceed"])
                && !EXTRA.iter().any(|w| rest.contains(w))
        })
    }

    /// La de rechazar: la primera que empieza con no.
    pub fn reject(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|o| starts_with_word(o, &["no", "deny", "reject", "cancel"]))
    }

    /// Las teclas para elegir la opción `i`: flechas desde el cursor y Enter.
    pub fn keys_for(&self, i: usize) -> Vec<String> {
        let (key, n) = if i >= self.cursor {
            ("Down", i - self.cursor)
        } else {
            ("Up", self.cursor - i)
        };
        let mut keys = vec![key.to_string(); n];
        keys.push("Enter".into());
        keys
    }
}

/// La huella del diálogo con la que se compara al usar el botón: lo que
/// pregunta y sus opciones. No la de la pantalla entera: el pie cambia solo
/// (pistas, la línea de estado) y el botón no serviría nunca. Tampoco el
/// cursor: las flechas se calculan al pulsar, desde donde esté.
fn hash_of(d: &Dialog) -> u64 {
    let text = format!("{}\n--\n{}", d.question.join("\n"), d.options.join("\n"));
    hooks::fnv1a(text.as_bytes())
}

/// Un botón pendiente.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Pending {
    token: String,
    /// Los botones del mismo aviso: usar uno anula los demás.
    group: String,
    agent: String,
    /// Lo que se eligió, para decirlo al contestar.
    label: String,
    /// Cuál de las opciones.
    option: usize,
    /// La huella del diálogo (`hash_of`).
    dialog: u64,
    expires: DateTime<Utc>,
}

fn load(path: &Path) -> Vec<Pending> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Lee, cambia y guarda los botones con el candado tomado.
fn update<T>(dir: &Path, f: impl FnOnce(&mut Vec<Pending>) -> T) -> Result<T> {
    fs::create_dir_all(dir)?;
    let _lock = store::lock(&dir.join("acciones.lock"))?;
    let path = dir.join("acciones.json");
    let mut list = load(&path);
    let now = Utc::now();
    list.retain(|p| p.expires > now);
    let out = f(&mut list);
    // Son llaves: que solo las lea el dueño.
    write_atomic_mode(&path, &serde_json::to_vec(&list)?, Some(0o600))?;
    Ok(out)
}

fn token() -> Result<String> {
    store::random_hex(16)
}

/// Un botón para un aviso: qué dice y su token.
pub struct Button {
    pub label: &'static str,
    pub token: String,
}

/// Los botones de aprobar y rechazar para el diálogo que muestra `screen`
/// del agente `agent`, si se entiende cuáles son. Con el diálogo, para
/// decir en el aviso qué se pregunta.
pub fn buttons(agent: &str, screen: &str) -> Option<(Dialog, Vec<Button>)> {
    buttons_in(&data_dir(), agent, screen)
}

fn buttons_in(dir: &Path, agent: &str, screen: &str) -> Option<(Dialog, Vec<Button>)> {
    let dialog = parse(screen)?;
    let choices: Vec<(&'static str, usize)> =
        [("Aprobar", dialog.approve()), ("Rechazar", dialog.reject())]
            .into_iter()
            .filter_map(|(l, i)| Some((l, i?)))
            .collect();
    if choices.is_empty() {
        return None;
    }
    let group = token().ok()?;
    let hash = hash_of(&dialog);
    let expires = Utc::now() + Duration::minutes(TTL_MINUTES);
    let mut buttons = Vec::new();
    let pending: Vec<Pending> = choices
        .iter()
        .filter_map(|&(label, i)| {
            let t = token().ok()?;
            buttons.push(Button {
                label,
                token: t.clone(),
            });
            Some(Pending {
                token: t,
                group: group.clone(),
                agent: agent.to_string(),
                label: bare(&dialog.options[i]).to_string(),
                option: i,
                dialog: hash,
                expires,
            })
        })
        .collect();
    update(dir, |list| {
        // Los de un aviso anterior del mismo agente ya no sirven.
        list.retain(|p| p.agent != agent);
        list.extend(pending);
    })
    .ok()?;
    Some((dialog, buttons))
}

/// Usa el botón `token`: si el diálogo sigue igual, pulsa las teclas de su
/// opción. Devuelve qué se eligió. El botón (y los de su aviso) se gastan
/// aunque no se pulse nada.
#[cfg_attr(not(feature = "serve"), allow(dead_code))]
pub fn act(token: &str) -> Result<String> {
    act_in(
        &data_dir(),
        token,
        crate::ops::screen,
        crate::ops::send_keys,
    )
}

#[cfg_attr(not(feature = "serve"), allow(dead_code))]
fn act_in(
    dir: &Path,
    token: &str,
    screen: impl FnOnce(&str) -> Result<String>,
    send: impl FnOnce(&str, &[String]) -> Result<()>,
) -> Result<String> {
    let found = update(dir, |list| {
        let p = list
            .iter()
            .find(|p| store::same(p.token.as_bytes(), token.as_bytes()))
            .cloned()?;
        list.retain(|q| q.group != p.group);
        Some(p)
    })?;
    let Some(p) = found else {
        bail!("este botón ya se usó o caducó");
    };
    let now = screen(&p.agent)?;
    let Some(d) = parse(&now).filter(|d| hash_of(d) == p.dialog) else {
        bail!("el diálogo ya cambió; ábrelo en la app para ver qué pide ahora");
    };
    send(&p.agent, &d.keys_for(p.option))?;
    Ok(format!("Elegido: {}", p.label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// La pantalla de un permiso de Bash de Claude Code, tal cual.
    const PERMISSION: &str = "\
● Deleting hola.txt
  ⎿ $ rm hola.txt

────────────────────────────────────────────────────────────
 Bash command
 Tip: auto mode handles prompts you — choose \"switch auto mode\" below
 Delete hola.txt and confirm removal
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 rm hola.txt && ls hola.txt 2>&1
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and always allow access to /tmp/x/scratchpad/perm-
      long from this project
   3. Yes, and switch to auto mode · auto mode handles these prompts for you
   4. No

 Esc to cancel · Tab to amend";

    /// El de confianza: la opción marcada es la de no.
    const TRUST: &str = "\
 Quick safety check: Is this a project you created or one you trust?
 Claude Code'll be able to read, edit, and execute files here.

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel";

    #[test]
    fn lee_el_permiso_de_claude() {
        let d = parse(PERMISSION).unwrap();
        assert_eq!(d.cursor, 0);
        assert_eq!(d.options.len(), 4);
        assert_eq!(
            d.options[1],
            "2. Yes, and always allow access to /tmp/x/scratchpad/perm- long from this project"
        );
        assert_eq!(
            d.question,
            [
                "Bash command",
                "Tip: auto mode handles prompts you — choose \"switch auto mode\" below",
                "Delete hola.txt and confirm removal",
                "rm hola.txt && ls hola.txt 2>&1",
                "Do you want to proceed?"
            ]
        );
        assert_eq!(d.approve(), Some(0));
        assert_eq!(d.keys_for(0), ["Enter"]);
        assert_eq!(d.reject(), Some(3));
        assert_eq!(d.keys_for(3), ["Down", "Down", "Down", "Enter"]);
    }

    #[test]
    fn en_el_de_confianza_aprobar_no_es_enter() {
        let d = parse(TRUST).unwrap();
        assert_eq!(d.options, ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(d.approve(), Some(1));
        assert_eq!(d.keys_for(1), ["Down", "Enter"]);
        assert_eq!(d.reject(), Some(0));
        assert_eq!(d.keys_for(0), ["Enter"]);
    }

    #[test]
    fn no_aprueba_un_si_que_da_mas_permisos() {
        let plan = " Would you like to proceed?\n ❯ 1. Yes, and bypass permissions\n   2. Yes, and manually approve edits\n   3. No, keep planning";
        let d = parse(plan).unwrap();
        assert_eq!(d.approve(), None);
        assert_eq!(d.reject(), Some(2));
        let codex = " Allow command?\n › 1. Yes, and don't ask again for this command\n   2. Yes, proceed\n   3. No, and tell Codex what to do differently";
        assert_eq!(parse(codex).unwrap().approve(), Some(1));
        // En el permiso de Claude, solo «1. Yes»: las demás guardan permisos.
        let d = parse(PERMISSION).unwrap();
        assert_eq!(d.approve(), Some(0));
    }

    #[test]
    fn sin_menu_no_hay_botones() {
        assert_eq!(parse("❯ \n────\n  ? for shortcuts"), None);
        assert_eq!(parse("hola\n❯ una sola opción"), None);
        // Opciones sin sí ni no: no se adivina.
        let d = parse(" ¿Qué modelo?\n ❯ Opus\n   Sonnet").unwrap();
        assert_eq!((d.approve(), d.reject()), (None, None));
    }

    #[test]
    fn dentro_de_una_caja() {
        let boxed = "╭──────────────╮\n│ Do you want to proceed? │\n│ ❯ 1. Yes     │\n│   2. No      │\n╰──────────────╯";
        let d = parse(boxed).unwrap();
        assert_eq!(d.options, ["1. Yes", "2. No"]);
        assert_eq!(d.question, ["Do you want to proceed?"]);
    }

    #[test]
    fn un_boton_sirve_una_vez_y_solo_con_el_mismo_dialogo() {
        let dir = std::env::temp_dir().join(format!("pastor-acciones-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sent = RefCell::new(Vec::new());
        let send = |a: &str, k: &[String]| {
            sent.borrow_mut().push((a.to_string(), k.to_vec()));
            Ok(())
        };
        let (_, b) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        assert_eq!(
            b.iter().map(|b| b.label).collect::<Vec<_>>(),
            ["Aprobar", "Rechazar"]
        );
        let reject = &b[1].token;
        let got = act_in(&dir, reject, |_| Ok(PERMISSION.into()), send).unwrap();
        assert_eq!(got, "Elegido: No");
        assert_eq!(sent.borrow()[0].1, ["Down", "Down", "Down", "Enter"]);
        // Gastado, y el de aprobar del mismo aviso también.
        assert!(act_in(&dir, reject, |_| Ok(PERMISSION.into()), send).is_err());
        assert!(act_in(&dir, &b[0].token, |_| Ok(PERMISSION.into()), send).is_err());

        // Si el cursor se movió, las flechas salen de donde está ahora.
        let (_, b) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        let moved = PERMISSION
            .replace(" ❯ 1. Yes", "   1. Yes")
            .replace("   4. No", " ❯ 4. No");
        act_in(&dir, &b[0].token, |_| Ok(moved.clone()), send).unwrap();
        assert_eq!(sent.borrow()[1].1, ["Up", "Up", "Up", "Enter"]);
        sent.borrow_mut().pop();
        // Si pregunta otra cosa, no pulsa nada.
        let (_, b) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        let other = PERMISSION.replace("rm hola.txt &&", "rm -rf / &&");
        let err = act_in(&dir, &b[0].token, |_| Ok(other.clone()), send).unwrap_err();
        assert!(err.to_string().contains("ya cambió"), "{err}");
        // Lo que cambia solo en el pie no cuenta.
        let (_, b) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        let footer = format!("{PERMISSION}\n  ⏸ manual mode on · 12:04");
        assert!(act_in(&dir, &b[0].token, |_| Ok(footer.clone()), send).is_ok());
        assert_eq!(sent.borrow().len(), 2);
        // Sin diálogo (ya contestaste), tampoco.
        let (_, b) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        assert!(act_in(&dir, &b[0].token, |_| Ok("❯ \n────".into()), send).is_err());
        assert_eq!(sent.borrow().len(), 2);
        // Un aviso nuevo del mismo agente anula los botones del anterior.
        let (_, old) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        let (_, new) = buttons_in(&dir, "a1", PERMISSION).unwrap();
        assert!(act_in(&dir, &old[0].token, |_| Ok(PERMISSION.into()), send).is_err());
        assert!(act_in(&dir, &new[0].token, |_| Ok(PERMISSION.into()), send).is_ok());
        assert_eq!(sent.borrow().len(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }
}
