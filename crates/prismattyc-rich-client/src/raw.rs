//! Raw / noecho stdin for PTY-attached clients (capability-protocol.md).

use std::io;

/// Enter raw / noecho when stdin is a TTY. Returns `None` on pipes (CI).
pub fn enter_raw_stdin() -> Option<RawStdin> {
    RawStdin::enter().ok()
}

/// Restores the previous termios on drop so a clean exit does not leave
/// the PTY stuck in raw mode. Crash paths still need the host to reset
/// the child PTY (it does, on pane close).
pub struct RawStdin {
    fd: i32,
    original: libc::termios,
}

impl RawStdin {
    /// Disable `ICANON` and `ECHO` on stdin when it is a TTY.
    ///
    /// # Errors
    ///
    /// Returns an error when stdin is not a TTY or `tcgetattr`/`tcsetattr` fails.
    pub fn enter() -> io::Result<Self> {
        let fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(fd) } == 0 {
            return Err(io::Error::other("stdin is not a TTY"));
        }
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawStdin {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}
