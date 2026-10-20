//! Un proceso en un pseudoterminal propio, para la terminal del celular:
//! `pastor serve` le pasa lo que se escribe y reenvía lo que dibuja.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

pub struct Pty {
    master: File,
    child: Child,
}

impl Pty {
    /// Lanza `cmd` con el pseudoterminal como terminal de control, del
    /// tamaño dado.
    pub fn spawn(mut cmd: Command, cols: u16, rows: u16) -> Result<Pty> {
        let (master, slave) = open_pair()?;
        Pty::resize_fd(&master, cols, rows).context("no pude darle tamaño al pseudoterminal")?;
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env("TERM", "xterm-256color")
            .env_remove("TMUX");
        // SAFETY: entre `fork` y `exec` solo llamadas al sistema seguras.
        unsafe {
            cmd.pre_exec(|| {
                // Una sesión nueva con el pseudoterminal como terminal de
                // control: así le llegan el tamaño (SIGWINCH) y el cierre.
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("no pude lanzar tmux")?;
        Ok(Pty { master, child })
    }

    /// Otro descriptor para leer lo que dibuja, desde otro hilo. Da error
    /// (EIO) o fin cuando el proceso termina.
    pub fn reader(&self) -> Result<File> {
        Ok(self.master.try_clone()?)
    }

    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        (&self.master).write_all(data)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        Pty::resize_fd(&self.master, cols, rows)
    }

    fn resize_fd(master: &File, cols: u16, rows: u16) -> io::Result<()> {
        let size = winsize(cols, rows);
        // SAFETY: `TIOCSWINSZ` solo lee el `winsize`.
        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Cierra el proceso y espera a que salga: bloquea un momento.
impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Abre un pseudoterminal: el lado de pastor y el del proceso. Los dos con
/// `O_CLOEXEC` desde que existen: si no, un proceso lanzado justo entonces
/// desde otro hilo (el servidor de tmux, que se queda) los heredaría y el
/// pseudoterminal nunca vería que nadie escucha.
#[cfg(target_os = "linux")]
fn open_pair() -> Result<(File, OwnedFd)> {
    // SAFETY: `posix_openpt` devuelve un descriptor nuevo o -1.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("no pude abrir un pseudoterminal");
    }
    // SAFETY: acaba de abrirse y nadie más es su dueño.
    let master = unsafe { File::from_raw_fd(fd) };
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: llamadas sobre el descriptor abierto; `ptsname_r` escribe a
    // lo más `name.len()` bytes.
    let ok = unsafe {
        libc::grantpt(fd) == 0
            && libc::unlockpt(fd) == 0
            && libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) == 0
    };
    if !ok {
        return Err(io::Error::last_os_error()).context("no pude preparar el pseudoterminal");
    }
    // SAFETY: `ptsname_r` dejó un nombre terminado en cero.
    let sfd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if sfd < 0 {
        return Err(io::Error::last_os_error()).context("no pude abrir el pseudoterminal");
    }
    // SAFETY: acaba de abrirse y nadie más es su dueño.
    Ok((master, unsafe { OwnedFd::from_raw_fd(sfd) }))
}

/// En macOS no hay `ptsname_r` ni `posix_openpt` con `O_CLOEXEC`: `openpty`
/// y la marca justo después. Queda un instante en que otro hilo podría
/// heredarlos.
#[cfg(not(target_os = "linux"))]
fn open_pair() -> Result<(File, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: `openpty` solo escribe los dos descriptores que le pasamos.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error()).context("no pude abrir un pseudoterminal");
    }
    // SAFETY: `openpty` acaba de abrirlos y nadie más es su dueño; `fcntl`
    // sobre descriptores abiertos.
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)))
    }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows.clamp(5, 500),
        ws_col: cols.clamp(20, 500),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn corre_en_su_terminal_con_su_tamano() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "stty size; tty -s && echo con-tty; read l; echo \"leí $l\"",
        ]);
        let pty = Pty::spawn(cmd, 77, 23).unwrap();
        let mut reader = pty.reader().unwrap();
        pty.write(b"hola\n").unwrap();
        let mut out = String::new();
        let mut buf = [0u8; 1024];
        // Hasta que sale: Linux da EIO al cerrarse el otro lado.
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
            if out.contains("leí hola") {
                break;
            }
        }
        assert!(out.contains("23 77"), "{out}");
        assert!(out.contains("con-tty"), "{out}");
        assert!(out.contains("leí hola"), "{out}");
    }
}
