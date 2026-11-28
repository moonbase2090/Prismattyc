//! Windows pane holders.
//!
//! A ConPTY handle cannot move to another process, and closing it ends the
//! program on it. So on Windows each pane's pseudoconsole and program belong
//! to a small holder process, `pmuxd --pty-holder`, instead of to pmuxd. The
//! server reaches the holder over two one-way named pipes (`-in` from the
//! server, `-out` to it); synchronous I/O on one pipe handle would make a
//! blocked read stall every write. Frames are in [`crate::pty_holder_frame`].
//!
//! A holder trusts a connection only after the server sends the random token
//! the holder was started with. With no server connected it stops reading
//! the console, so output waits in ConPTY. When the pipe closes it keeps the
//! program for a grace period (longer after a handoff notice) and ends it if
//! no server returns.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::pty_holder_frame::TOKEN_LEN;

/// Grace after a server disconnects without a handoff notice. Long enough
/// for a pipe hiccup, short enough that a crashed or stopped server still
/// ends its panes promptly.
pub const DISCONNECT_GRACE_MS: u32 = 3_000;

/// What the server sends a new holder on its standard input, as one JSON line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderSpec {
    /// Base name; the pipes are `\\.\pipe\<pipe>-in` and `-out`.
    pub pipe: String,
    /// Hex of [`TOKEN_LEN`] random bytes.
    pub token: String,
    pub program: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub grace_ms: u32,
}

/// Full pipe paths for a base name: (server to holder, holder to server).
pub fn pipe_paths(base: &str) -> (String, String) {
    (
        format!(r"\\.\pipe\{base}-in"),
        format!(r"\\.\pipe\{base}-out"),
    )
}

/// A base name unique to this server and pane: no other process can guess it
/// to squat on the pipe before the holder creates it.
pub fn pipe_base(server_pid: u32, nonce: &[u8; 8]) -> String {
    format!("pmux-holder-{server_pid}-{}", hex(nonce))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Decode a token written by [`hex`]. `None` for anything else.
pub fn token_from_hex(text: &str) -> Option<[u8; TOKEN_LEN]> {
    if text.len() != TOKEN_LEN * 2 || !text.is_ascii() {
        return None;
    }
    let mut token = [0u8; TOKEN_LEN];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(token)
}

/// Compare tokens without stopping at the first difference.
pub fn tokens_match(a: &[u8; TOKEN_LEN], b: &[u8; TOKEN_LEN]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// When a holder reports its program's exit. A session reports the exit
/// only once (reading it reaps the program), and the report waits until the
/// program's last output has gone out, so it must be kept once seen.
#[cfg(any(windows, test))]
#[derive(Debug, Default)]
pub(crate) struct ExitReport {
    code: Option<u32>,
}

#[cfg(any(windows, test))]
impl ExitReport {
    /// Record the exit the first time `poll` returns one; skip `poll` after.
    pub(crate) fn observe(&mut self, poll: impl FnOnce() -> Option<u32>) {
        if self.code.is_none() {
            self.code = poll();
        }
    }

    /// The exit code to send, once the output is quiet.
    pub(crate) fn ready(&self, quiet: bool) -> Option<u32> {
        self.code.filter(|_| quiet)
    }
}

#[cfg(windows)]
pub use self::windows::{run_holder, spawn_holder};

#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Context, Result};
    use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
    use prismattyc_emulator::PtySession;
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
        PIPE_ACCESS_OUTBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    };

    use super::{
        hex, pipe_base, pipe_paths, token_from_hex, tokens_match, ExitReport, HolderSpec,
        DISCONNECT_GRACE_MS,
    };
    use crate::platform::UserOnlyDescriptor;
    use crate::pty_holder_frame::{read_frame, write_frame, Frame, HOLDER_PROTOCOL, TOKEN_LEN};

    const PIPE_BUFFER: u32 = 64 * 1024;
    /// How long a new holder may take to report that its pipes exist.
    const START_TIMEOUT: Duration = Duration::from_secs(10);
    /// Quiet output after the program exits before the exit is reported,
    /// so its last output reaches the server first.
    const EXIT_QUIET: Duration = Duration::from_millis(50);
    const EXIT_POLL: Duration = Duration::from_millis(25);
    /// How long closing a pane waits for its holder to report the exit. The
    /// report arrives through the pane's output reader; if that reader is
    /// already gone, the pane still closes.
    const KILL_WAIT: Duration = Duration::from_secs(5);

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// One end of a holder pipe, created by the holder.
    fn create_pipe(path: &str, access: u32) -> io::Result<File> {
        let descriptor = UserOnlyDescriptor::new()?;
        let attributes = descriptor.attributes();
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated and `attributes` points at a live
        // descriptor for the duration of the call.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                access | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle this function owns.
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    /// Wait for the server on a holder-created pipe.
    fn accept(pipe: &File) -> io::Result<()> {
        // SAFETY: `pipe` is a live named pipe server handle.
        if unsafe { ConnectNamedPipe(pipe.as_raw_handle() as _, std::ptr::null_mut()) } != 0 {
            return Ok(());
        }
        // SAFETY: reads the calling thread's last error.
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn disconnect(pipe: &File) {
        // SAFETY: `pipe` is a live named pipe server handle.
        unsafe { DisconnectNamedPipe(pipe.as_raw_handle() as _) };
    }

    /// Open one end of a holder pipe from the server.
    fn open_pipe(path: &str, access: u32, deadline: Instant) -> io::Result<File> {
        let name = wide(path);
        loop {
            // SAFETY: `name` is NUL-terminated; no security attributes.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    access,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                // SAFETY: a fresh handle this function owns.
                return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
            }
            let err = io::Error::last_os_error();
            if Instant::now() >= deadline {
                return Err(err);
            }
            if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                // SAFETY: `name` is NUL-terminated.
                unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
            } else {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::{
            ImpersonateLoggedOnUser, LogonUserW, RevertToSelf, DACL_SECURITY_INFORMATION,
            LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT,
        };

        /// A descriptor's DACL as SDDL text. Windows writes well-known SIDs
        /// by alias (the built-in Administrator account is `LA`), so expected
        /// text must go through here too before it is compared.
        fn dacl_text(descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR) -> String {
            let mut text = std::ptr::null_mut();
            // SAFETY: `descriptor` is a valid security descriptor.
            let ok = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(
                ok, 0,
                "ConvertSecurityDescriptorToStringSecurityDescriptorW"
            );
            let mut len = 0;
            // SAFETY: `text` is a NUL-terminated string from the call above.
            while unsafe { *text.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: `len` UTF-16 units were just measured.
            let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
            // SAFETY: LocalAlloc'd by the call above.
            unsafe { windows_sys::Win32::Foundation::LocalFree(text.cast()) };
            sddl
        }

        /// `sddl` as Windows itself would print it.
        fn canonical(sddl: &str) -> String {
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: NUL-terminated input; the descriptor is LocalAlloc'd.
            let ok = unsafe {
                windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(sddl).as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "parse {sddl}");
            let text = dacl_text(descriptor);
            // SAFETY: LocalAlloc'd by the call above.
            unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };
            text
        }

        fn pipe_dacl(pipe: &File) -> String {
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: a live pipe handle; the descriptor is LocalAlloc'd.
            let status = unsafe {
                GetSecurityInfo(
                    pipe.as_raw_handle() as HANDLE,
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(status, 0, "GetSecurityInfo");
            let sddl = dacl_text(descriptor);
            // SAFETY: LocalAlloc'd by GetSecurityInfo.
            unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };
            sddl
        }

        fn test_pipe_path() -> String {
            let mut nonce = [0u8; 8];
            getrandom::fill(&mut nonce).unwrap();
            pipe_paths(&pipe_base(std::process::id(), &nonce)).0
        }

        /// The holder's pipe admits only the user who runs the server: its
        /// DACL is protected and has one entry, for that user. When the run
        /// provides another local account (`PMUX_TEST_OTHER_USER` and
        /// `PMUX_TEST_OTHER_PASSWORD`, as the clean-VM proof does), opening
        /// the pipe as that account is refused while this user can open it.
        #[test]
        fn a_holder_pipe_admits_only_the_current_user() {
            let path = test_pipe_path();
            let pipe = create_pipe(&path, PIPE_ACCESS_INBOUND).unwrap();
            let sid = crate::platform::current_user_sid().unwrap();
            let dacl = pipe_dacl(&pipe);
            assert_eq!(dacl, canonical(&format!("D:P(A;;FA;;;{sid})")));
            assert_eq!(dacl.matches("(A;").count(), 1, "one allow entry: {dacl}");
            assert!(
                dacl.starts_with("D:P"),
                "protected from inherited entries: {dacl}"
            );

            if let (Ok(user), Ok(password)) = (
                std::env::var("PMUX_TEST_OTHER_USER"),
                std::env::var("PMUX_TEST_OTHER_PASSWORD"),
            ) {
                let mut token: HANDLE = std::ptr::null_mut();
                // SAFETY: NUL-terminated strings; `token` receives a handle.
                let logged_on = unsafe {
                    LogonUserW(
                        wide(&user).as_ptr(),
                        wide(".").as_ptr(),
                        wide(&password).as_ptr(),
                        LOGON32_LOGON_NETWORK,
                        LOGON32_PROVIDER_DEFAULT,
                        &mut token,
                    )
                };
                assert_ne!(
                    logged_on,
                    0,
                    "log on as {user}: {}",
                    io::Error::last_os_error()
                );
                // SAFETY: `token` is a fresh logon token.
                assert_ne!(unsafe { ImpersonateLoggedOnUser(token) }, 0);
                let refused = open_pipe(&path, GENERIC_WRITE, Instant::now());
                // SAFETY: undo the impersonation on this thread.
                unsafe {
                    RevertToSelf();
                    CloseHandle(token);
                }
                let err = refused.expect_err("another user opened the holder pipe");
                assert_eq!(
                    err.raw_os_error(),
                    Some(ERROR_ACCESS_DENIED as i32),
                    "{err}"
                );
                eprintln!("checked: {user} cannot open the holder pipe");
            }
            open_pipe(&path, GENERIC_WRITE, Instant::now()).expect("the owner opens it");
        }
    }

    // ---- the holder process ------------------------------------------

    struct Link {
        /// The server's end for output, while a server is connected.
        out: Mutex<Option<File>>,
        connected: Condvar,
        /// The pipe closes after this many milliseconds without a server.
        grace_ms: AtomicU32,
        /// Bumped on each connection, so a stale grace timer does nothing.
        generation: AtomicU64,
        last_output: Mutex<Instant>,
    }

    impl Link {
        /// Write a frame to the connected server, waiting for one if needed.
        /// A failed write drops the connection and waits for the next.
        fn send(&self, frame: &Frame) {
            let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(pipe) = out.as_mut() {
                    if write_frame(pipe, frame).and_then(|()| pipe.flush()).is_ok() {
                        return;
                    }
                    *out = None;
                }
                out = self.connected.wait(out).unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    /// `pmuxd --pty-holder`: own one pane's pseudoconsole and program.
    pub fn run_holder() -> Result<()> {
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .context("read holder spec")?;
        let spec: HolderSpec = serde_json::from_str(&line).context("parse holder spec")?;
        let token = token_from_hex(&spec.token).context("holder token")?;
        let (in_path, out_path) = pipe_paths(&spec.pipe);
        let inbound = create_pipe(&in_path, PIPE_ACCESS_INBOUND).context("create input pipe")?;
        let outbound =
            create_pipe(&out_path, PIPE_ACCESS_OUTBOUND).context("create output pipe")?;

        let size = PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: spec.pixel_width,
            pixel_height: spec.pixel_height,
        };
        let mut session = PtySession::spawn_config(
            &spec.program,
            &spec.args,
            spec.cwd.as_deref(),
            &spec.env,
            size,
        )
        .context("start pane program")?;
        let pid = session.process_id().unwrap_or(0);
        let mut console = session.take_reader()?;
        let mut input = session.take_input_writer()?;
        let session = Arc::new(Mutex::new(session));

        // The server waits for this line before it opens the pipes.
        {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "ready {pid}")?;
            stdout.flush()?;
        }

        let link = Arc::new(Link {
            out: Mutex::new(None),
            connected: Condvar::new(),
            grace_ms: AtomicU32::new(spec.grace_ms.max(1)),
            generation: AtomicU64::new(0),
            last_output: Mutex::new(Instant::now()),
        });

        // Console output goes to the server only while one is connected; the
        // bounded channel then stops this reader, so output waits in ConPTY.
        let (chunks, forward) = mpsc::sync_channel::<Vec<u8>>(4);
        {
            let link = link.clone();
            thread::Builder::new()
                .name("holder-console".into())
                .spawn(move || {
                    let mut buffer = vec![0u8; 16 * 1024];
                    loop {
                        match console.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                *link.last_output.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Instant::now();
                                if chunks.send(buffer[..n].to_vec()).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })?;
        }
        let exited = Arc::new(AtomicBool::new(false));
        {
            let link = link.clone();
            let session = session.clone();
            let exited = exited.clone();
            thread::Builder::new()
                .name("holder-forward".into())
                .spawn(move || {
                    let mut ended = ExitReport::default();
                    loop {
                        match forward.recv_timeout(EXIT_POLL) {
                            Ok(bytes) => link.send(&Frame::Output(bytes)),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            // The console reached end of file; keep polling the exit.
                            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(EXIT_POLL),
                        }
                        ended.observe(|| {
                            session
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .try_wait()
                                .ok()
                                .flatten()
                                .map(|status| status.exit_code())
                        });
                        let quiet = link
                            .last_output
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .elapsed()
                            >= EXIT_QUIET;
                        let Some(code) = ended.ready(quiet) else {
                            continue;
                        };
                        while let Ok(bytes) = forward.try_recv() {
                            link.send(&Frame::Output(bytes));
                        }
                        link.send(&Frame::Exit { code });
                        exited.store(true, Ordering::Release);
                        // Give the server a moment to read the exit, then go.
                        thread::sleep(Duration::from_millis(200));
                        std::process::exit(0);
                    }
                })?;
        }

        loop {
            // Each connection: both ends, then the token.
            if accept(&inbound).is_err() || accept(&outbound).is_err() {
                disconnect(&inbound);
                disconnect(&outbound);
                continue;
            }
            let mut reader = inbound.try_clone()?;
            let trusted = match read_frame(&mut reader) {
                Ok(Some(Frame::Hello {
                    protocol,
                    token: offered,
                })) => protocol == HOLDER_PROTOCOL && tokens_match(&offered, &token),
                _ => false,
            };
            if !trusted {
                disconnect(&inbound);
                disconnect(&outbound);
                continue;
            }
            {
                let mut out = outbound.try_clone()?;
                write_frame(
                    &mut out,
                    &Frame::Ready {
                        protocol: HOLDER_PROTOCOL,
                        pid,
                    },
                )?;
                out.flush()?;
                link.generation.fetch_add(1, Ordering::AcqRel);
                link.grace_ms.store(spec.grace_ms.max(1), Ordering::Release);
                *link.out.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
                link.connected.notify_all();
            }
            // Serve this server until its pipe closes.
            loop {
                match read_frame(&mut reader) {
                    Ok(Some(Frame::Input(bytes))) => {
                        if input
                            .write_all(&bytes)
                            .and_then(|()| input.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Some(Frame::Resize {
                        cols,
                        rows,
                        pixel_width,
                        pixel_height,
                    })) => {
                        let _ = session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .resize(PtySize {
                                rows,
                                cols,
                                pixel_width,
                                pixel_height,
                            });
                    }
                    Ok(Some(Frame::Kill)) => {
                        session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .kill_and_reap();
                    }
                    Ok(Some(Frame::Handoff { grace_ms })) => {
                        link.grace_ms.store(grace_ms.max(1), Ordering::Release);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            // The server is gone. Stop forwarding and start the grace timer.
            *link.out.lock().unwrap_or_else(|e| e.into_inner()) = None;
            disconnect(&inbound);
            disconnect(&outbound);
            if exited.load(Ordering::Acquire) {
                return Ok(());
            }
            let generation = link.generation.load(Ordering::Acquire);
            let grace = Duration::from_millis(u64::from(link.grace_ms.load(Ordering::Acquire)));
            let link = link.clone();
            let session = session.clone();
            thread::Builder::new()
                .name("holder-grace".into())
                .spawn(move || {
                    thread::sleep(grace);
                    if link.generation.load(Ordering::Acquire) == generation {
                        session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .kill_and_reap();
                        std::process::exit(0);
                    }
                })?;
        }
    }

    // ---- the server side ---------------------------------------------

    #[derive(Default)]
    struct ExitState {
        code: Mutex<Option<u32>>,
        changed: Condvar,
    }

    impl ExitState {
        fn set(&self, code: u32) {
            let mut slot = self.code.lock().unwrap_or_else(|e| e.into_inner());
            slot.get_or_insert(code);
            self.changed.notify_all();
        }
    }

    /// The server's connection to one holder.
    struct Connection {
        to_holder: Mutex<File>,
        exit: ExitState,
        /// Base pipe name and token, kept for a later handoff record.
        #[allow(dead_code)]
        pipe: String,
        #[allow(dead_code)]
        token: [u8; TOKEN_LEN],
        #[allow(dead_code)]
        holder_pid: u32,
    }

    impl Connection {
        fn send(&self, frame: &Frame) -> io::Result<()> {
            let mut pipe = self.to_holder.lock().unwrap_or_else(|e| e.into_inner());
            write_frame(&mut *pipe, frame)?;
            pipe.flush()
        }
    }

    struct HolderMaster {
        connection: Arc<Connection>,
        from_holder: Mutex<Option<File>>,
        size: Mutex<PtySize>,
    }

    struct HolderReader {
        connection: Arc<Connection>,
        pipe: File,
        pending: Vec<u8>,
    }

    impl Read for HolderReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            while self.pending.is_empty() {
                match read_frame(&mut self.pipe) {
                    Ok(Some(Frame::Output(bytes))) => self.pending = bytes,
                    Ok(Some(Frame::Exit { code })) => {
                        self.connection.exit.set(code);
                        return Ok(0);
                    }
                    Ok(Some(_)) => {}
                    // The holder is gone without reporting an exit.
                    Ok(None) | Err(_) => {
                        self.connection.exit.set(1);
                        return Ok(0);
                    }
                }
            }
            let n = buffer.len().min(self.pending.len());
            buffer[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }

    struct HolderWriter(Arc<Connection>);

    impl Write for HolderWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.send(&Frame::Input(bytes.to_vec()))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl MasterPty for HolderMaster {
        fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
            *self.size.lock().unwrap_or_else(|e| e.into_inner()) = size;
            self.connection.send(&Frame::Resize {
                cols: size.cols,
                rows: size.rows,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })?;
            Ok(())
        }
        fn get_size(&self) -> Result<PtySize, anyhow::Error> {
            Ok(*self.size.lock().unwrap_or_else(|e| e.into_inner()))
        }
        fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
            let pipe = self
                .from_holder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| anyhow!("holder output reader was already taken"))?;
            Ok(Box::new(HolderReader {
                connection: self.connection.clone(),
                pipe,
                pending: Vec::new(),
            }))
        }
        fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
            Ok(Box::new(HolderWriter(self.connection.clone())))
        }
    }

    #[derive(Debug)]
    struct HolderChild {
        connection: Arc<Connection>,
        pid: u32,
    }

    impl std::fmt::Debug for Connection {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Connection")
                .field("pipe", &self.pipe)
                .finish_non_exhaustive()
        }
    }

    impl ChildKiller for HolderChild {
        fn kill(&mut self) -> io::Result<()> {
            match self.connection.send(&Frame::Kill) {
                Ok(()) => Ok(()),
                // A holder that is already gone has nothing left to kill.
                Err(_) => {
                    self.connection.exit.set(1);
                    Ok(())
                }
            }
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(HolderChild {
                connection: self.connection.clone(),
                pid: self.pid,
            })
        }
    }

    impl Child for HolderChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(self
                .connection
                .exit
                .code
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map(ExitStatus::with_exit_code))
        }
        fn wait(&mut self) -> io::Result<ExitStatus> {
            let deadline = Instant::now() + KILL_WAIT;
            let mut code = self
                .connection
                .exit
                .code
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(code) = *code {
                    return Ok(ExitStatus::with_exit_code(code));
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(ExitStatus::with_exit_code(1));
                }
                code = self
                    .connection
                    .exit
                    .changed
                    .wait_timeout(code, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        fn process_id(&self) -> Option<u32> {
            (self.pid != 0).then_some(self.pid)
        }
        fn as_raw_handle(&self) -> Option<RawHandle> {
            None
        }
    }

    /// Start a holder for one pane and connect to it. The returned session
    /// behaves like an in-process ConPTY session.
    pub fn spawn_holder(
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        env: &std::collections::BTreeMap<String, String>,
        size: PtySize,
    ) -> Result<PtySession> {
        let mut nonce = [0u8; 8];
        let mut token = [0u8; TOKEN_LEN];
        getrandom::fill(&mut nonce).map_err(|err| anyhow!("holder nonce: {err}"))?;
        getrandom::fill(&mut token).map_err(|err| anyhow!("holder token: {err}"))?;
        let pipe = pipe_base(std::process::id(), &nonce);
        let spec = HolderSpec {
            pipe: pipe.clone(),
            token: hex(&token),
            program: program.to_string(),
            args: args.to_vec(),
            cwd: cwd.map(std::path::Path::to_path_buf),
            env: env.clone(),
            cols: size.cols,
            rows: size.rows,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
            grace_ms: DISCONNECT_GRACE_MS,
        };
        let exe = std::env::current_exe().context("pmuxd path for the pane holder")?;
        let mut command = Command::new(exe);
        command
            .arg("--pty-holder")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Leave this server's job when allowed, so the holder can outlive it.
        let base_flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        // A holder outlives this server, so it must not inherit the server's
        // standard handles (its log file, say) and keep them open.
        command.creation_flags(base_flags | CREATE_BREAKAWAY_FROM_JOB);
        let mut holder = match crate::platform::spawn_without_parent_stdio(&mut command) {
            Ok(holder) => holder,
            // A job that forbids breakaway: start inside it.
            Err(_) => {
                crate::platform::spawn_without_parent_stdio(command.creation_flags(base_flags))
                    .context("start pane holder")?
            }
        };
        let holder_pid = holder.id();
        {
            let mut stdin = holder.stdin.take().context("holder stdin")?;
            serde_json::to_writer(&mut stdin, &spec)?;
            stdin.write_all(b"\n")?;
        }
        let stdout = holder.stdout.take().context("holder stdout")?;
        let (said, heard) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = said.send(line);
        });
        let line = heard
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| anyhow!("pane holder did not start"))?;
        let pid = line
            .trim()
            .strip_prefix("ready ")
            .and_then(|pid| pid.parse::<u32>().ok())
            .ok_or_else(|| anyhow!("pane holder failed to start the program"))?;
        // The holder outlives this handle; the pipe is how it is reached.
        drop(holder);

        let (in_path, out_path) = pipe_paths(&pipe);
        let deadline = Instant::now() + START_TIMEOUT;
        let mut to_holder = open_pipe(&in_path, GENERIC_WRITE, deadline)?;
        let mut from_holder = open_pipe(&out_path, GENERIC_READ, deadline)?;
        write_frame(
            &mut to_holder,
            &Frame::Hello {
                protocol: HOLDER_PROTOCOL,
                token,
            },
        )?;
        to_holder.flush()?;
        match read_frame(&mut from_holder)? {
            Some(Frame::Ready { protocol, .. }) if protocol == HOLDER_PROTOCOL => {}
            other => bail!("pane holder refused the connection: {other:?}"),
        }
        let connection = Arc::new(Connection {
            to_holder: Mutex::new(to_holder),
            exit: ExitState::default(),
            pipe,
            token,
            holder_pid,
        });
        let master = HolderMaster {
            connection: connection.clone(),
            from_holder: Mutex::new(Some(from_holder)),
            size: Mutex::new(size),
        };
        let child = HolderChild { connection, pid };
        PtySession::from_parts(Box::new(master), Box::new(child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_seen_while_output_is_still_coming_is_reported_later() {
        // A session reports the exit once, then reports nothing.
        let mut reports = vec![None, Some(7)].into_iter();
        let mut poll = move || reports.next().flatten();
        let mut exit = ExitReport::default();
        exit.observe(&mut poll);
        assert_eq!(exit.ready(true), None, "still running");
        exit.observe(&mut poll);
        assert_eq!(exit.ready(false), None, "output is not quiet yet");
        exit.observe(&mut poll);
        assert_eq!(exit.ready(true), Some(7), "the exit is kept once seen");
    }

    #[test]
    fn tokens_round_trip_through_hex_and_reject_anything_else() {
        let token: [u8; TOKEN_LEN] = std::array::from_fn(|i| (i * 37 % 256) as u8);
        let text = hex(&token);
        assert_eq!(text.len(), TOKEN_LEN * 2);
        assert_eq!(token_from_hex(&text), Some(token));
        assert_eq!(token_from_hex(&text[1..]), None, "short");
        assert_eq!(token_from_hex(&format!("{text}00")), None, "long");
        assert_eq!(token_from_hex(&"zz".repeat(TOKEN_LEN)), None, "not hex");
        assert_eq!(token_from_hex(&"é".repeat(TOKEN_LEN)), None, "not ascii");
    }

    #[test]
    fn token_comparison_needs_every_byte() {
        let a = [9u8; TOKEN_LEN];
        assert!(tokens_match(&a, &a));
        for index in 0..TOKEN_LEN {
            let mut b = a;
            b[index] ^= 1;
            assert!(!tokens_match(&a, &b), "differs at {index}");
        }
    }

    #[test]
    fn pipe_names_are_per_server_and_pane_and_local_only() {
        let base = pipe_base(4242, &[0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3]);
        assert_eq!(base, "pmux-holder-4242-deadbeef00010203");
        let (to_holder, from_holder) = pipe_paths(&base);
        assert_eq!(to_holder, r"\\.\pipe\pmux-holder-4242-deadbeef00010203-in");
        assert_eq!(
            from_holder,
            r"\\.\pipe\pmux-holder-4242-deadbeef00010203-out"
        );
    }

    #[test]
    fn a_holder_spec_survives_its_json_line() {
        let spec = HolderSpec {
            pipe: "pmux-holder-1-0000000000000000".into(),
            token: hex(&[1; TOKEN_LEN]),
            program: r"C:\Windows\System32\cmd.exe".into(),
            args: vec!["/k".into(), "echo hi".into()],
            cwd: Some(PathBuf::from(r"C:\work")),
            env: BTreeMap::from([("PRISMATTYC_PANE_ID".into(), "7".into())]),
            cols: 80,
            rows: 24,
            pixel_width: 640,
            pixel_height: 384,
            grace_ms: DISCONNECT_GRACE_MS,
        };
        let line = serde_json::to_string(&spec).unwrap();
        assert!(!line.contains('\n'), "the spec is one line on stdin");
        let back: HolderSpec = serde_json::from_str(&line).unwrap();
        assert_eq!(back, spec);
    }
}
