//! Native filesystem-addressed local sockets on Unix and Windows.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

/// Connect to `path`, giving up after `timeout`.
///
/// The returned stream is blocking, so a later `set_read_timeout` still waits.
/// `UnixStream::connect` has no connect deadline. The desktop host calls this
/// on its UI thread, and a stuck daemon connect stops that thread from pumping.
pub fn connect_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    connect_timeout_impl(path, timeout)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connect timed out")
}

#[cfg(unix)]
fn connect_pending(err: rustix::io::Errno) -> bool {
    // `EAGAIN` on a Linux unix socket means the listen queue is full and the
    // connect was not started. Waiting would sit out the whole deadline on a
    // socket that will never become connected.
    err == rustix::io::Errno::INPROGRESS
}

#[cfg(unix)]
fn wait_until_connected(sock: &std::os::fd::OwnedFd, timeout: Duration) -> io::Result<()> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use rustix::io::Errno;
    use rustix::net::sockopt::socket_error;

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let spec = Timespec::try_from(remaining)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut fds = [PollFd::new(sock, PollFlags::OUT)];
        match poll(&mut fds, Some(&spec)) {
            Ok(0) => return Err(timed_out()),
            Ok(_) => match socket_error(sock)? {
                Ok(()) => return Ok(()),
                Err(err) if connect_pending(err) => continue,
                Err(err) => return Err(err.into()),
            },
            Err(Errno::INTR) => continue,
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(unix)]
fn open_nonblocking_unix() -> io::Result<std::os::fd::OwnedFd> {
    use rustix::io::{fcntl_setfd, ioctl_fionbio, FdFlags};
    use rustix::net::{socket, AddressFamily, SocketType};

    // macOS has no `SOCK_CLOEXEC` or `SOCK_NONBLOCK`. Close-on-exec keeps a
    // child of the GUI from inheriting a connect that is still in progress.
    let sock = socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    fcntl_setfd(&sock, FdFlags::CLOEXEC)?;
    ioctl_fionbio(&sock, true)?;
    Ok(sock)
}

#[cfg(unix)]
fn connect_timeout_impl(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    use rustix::net::connect;

    let addr = rustix::net::SocketAddrUnix::new(path)?;
    let sock = open_nonblocking_unix()?;
    match connect(&sock, &addr) {
        Ok(()) => {}
        Err(err) if connect_pending(err) => wait_until_connected(&sock, timeout)?,
        Err(err) => return Err(err.into()),
    }
    // `SOCK_NONBLOCK` would make a later read return at once and ignore
    // `SO_RCVTIMEO`, which is how the host bounds a stalled daemon.
    rustix::io::ioctl_fionbio(&sock, false)?;
    Ok(UnixStream::from(sock))
}

#[cfg(windows)]
fn connect_timeout_impl(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    use std::mem::{self, size_of};
    use std::os::windows::io::FromRawSocket;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Networking::WinSock::{
        closesocket, connect, getsockopt, ioctlsocket, WSACloseEvent, WSACreateEvent,
        WSAEventSelect, WSAGetLastError, WSASocketW, WSAStartup, WSAWaitForMultipleEvents, AF_UNIX,
        FD_CONNECT, FIONBIO, INVALID_SOCKET, SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR,
        SOCK_STREAM, SOL_SOCKET, SO_ERROR, WSADATA, WSAEINPROGRESS, WSAEWOULDBLOCK,
        WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSA_INVALID_EVENT,
    };

    ensure_wsa()?;
    let raw = unsafe {
        WSASocketW(
            AF_UNIX as i32,
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if raw == INVALID_SOCKET {
        return Err(wsa_error());
    }
    let socket = SocketGuard(raw);
    let event_raw = unsafe { WSACreateEvent() };
    if event_raw == WSA_INVALID_EVENT {
        return Err(wsa_error());
    }
    let event = EventGuard(event_raw);
    // WSAEventSelect makes the socket nonblocking and arms FD_CONNECT.
    // WSAPoll does not reliably report AF_UNIX, so the wait is this event.
    if unsafe { WSAEventSelect(socket.0, event.0, FD_CONNECT as i32) } == SOCKET_ERROR {
        return Err(wsa_error());
    }
    let (addr, len) = sockaddr_un(path)?;
    let started = unsafe {
        connect(
            socket.0,
            &addr as *const SOCKADDR_UN as *const SOCKADDR,
            len,
        )
    };
    if started == SOCKET_ERROR {
        let err = unsafe { WSAGetLastError() };
        if err != WSAEWOULDBLOCK && err != WSAEINPROGRESS {
            return Err(io::Error::from_raw_os_error(err));
        }
        wait_for_connect(socket.0, event.0, timeout)?;
    }
    socket_error_ok(socket.0)?;
    return into_blocking_stream(socket);

    fn ensure_wsa() -> io::Result<()> {
        static STARTED: OnceLock<i32> = OnceLock::new();
        let code = *STARTED.get_or_init(|| unsafe {
            let mut data: WSADATA = mem::zeroed();
            WSAStartup(0x0202, &mut data)
        });
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    }

    fn wsa_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    fn sockaddr_un(path: &Path) -> io::Result<(SOCKADDR_UN, i32)> {
        let mut addr: SOCKADDR_UN = unsafe { mem::zeroed() };
        addr.sun_family = AF_UNIX;
        let bytes = path.to_str().map(|text| text.as_bytes()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains invalid characters",
            )
        })?;
        if bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "paths may not contain interior null bytes",
            ));
        }
        if bytes.len() >= addr.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must be shorter than SUN_LEN",
            ));
        }
        for (dst, src) in addr.sun_path.iter_mut().zip(bytes.iter()) {
            *dst = *src as i8;
        }
        let mut len = mem::offset_of!(SOCKADDR_UN, sun_path) + bytes.len();
        if bytes.first().is_some_and(|byte| *byte != 0) {
            len += 1;
        }
        Ok((addr, len as i32))
    }

    fn wait_for_connect(
        sock: SOCKET,
        event: windows_sys::Win32::Networking::WinSock::WSAEVENT,
        timeout: Duration,
    ) -> io::Result<()> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let handle = event as HANDLE;
        let waited = unsafe { WSAWaitForMultipleEvents(1, &handle, 0, millis, 0) };
        if waited == WAIT_OBJECT_0 {
            return Ok(());
        }
        if waited == WAIT_TIMEOUT {
            return Err(timed_out());
        }
        if waited == WAIT_FAILED {
            return Err(wsa_error());
        }
        Err(wsa_error())
    }

    fn socket_error_ok(sock: SOCKET) -> io::Result<()> {
        let mut code = 0i32;
        let mut len = size_of::<i32>() as i32;
        let rc = unsafe {
            getsockopt(
                sock,
                SOL_SOCKET,
                SO_ERROR,
                &mut code as *mut i32 as *mut u8,
                &mut len,
            )
        };
        if rc == SOCKET_ERROR {
            return Err(wsa_error());
        }
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    }

    fn into_blocking_stream(socket: SocketGuard) -> io::Result<UnixStream> {
        if unsafe { WSAEventSelect(socket.0, WSA_INVALID_EVENT, 0) } == SOCKET_ERROR {
            return Err(wsa_error());
        }
        let mut off = 0u32;
        if unsafe { ioctlsocket(socket.0, FIONBIO, &mut off) } == SOCKET_ERROR {
            return Err(wsa_error());
        }
        let raw = socket.0;
        mem::forget(socket);
        Ok(unsafe { UnixStream::from_raw_socket(raw as _) })
    }

    struct SocketGuard(SOCKET);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            unsafe {
                closesocket(self.0);
            }
        }
    }

    struct EventGuard(windows_sys::Win32::Networking::WinSock::WSAEVENT);
    impl Drop for EventGuard {
        fn drop(&mut self) {
            unsafe {
                WSACloseEvent(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{connect_timeout, UnixListener};

    struct Unlink(PathBuf);
    impl Drop for Unlink {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn socket_path(label: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!("pct-{}-{label}.sock", std::process::id()))
    }

    #[test]
    fn connect_timeout_does_not_wait_out_a_missing_socket() {
        let path = socket_path("missing");
        let _ = std::fs::remove_file(&path);
        let started = Instant::now();
        let error = connect_timeout(&path, Duration::from_secs(2)).unwrap_err();
        let waited = started.elapsed();
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::TimedOut,
            "missing socket must fail immediately, not as a timeout ({error})"
        );
        assert!(
            waited < Duration::from_millis(500),
            "missing socket waited {waited:?}"
        );
    }

    #[test]
    fn connect_timeout_reaches_a_listener_and_keeps_the_socket_blocking() {
        let path = socket_path("ok");
        let _unlink = Unlink(path.clone());
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, [b'Q']);
            stream.write_all(b"A").unwrap();
        });

        let mut client = connect_timeout(&path, Duration::from_secs(2)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let started = Instant::now();
        let error = client.read(&mut [0u8; 1]).unwrap_err();
        let waited = started.elapsed();
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ),
            "blocking read with a timeout returned {error}"
        );
        assert!(
            waited >= Duration::from_millis(120),
            "read returned in {waited:?}; the socket was still nonblocking"
        );
        assert!(
            waited < Duration::from_secs(2),
            "read waited {waited:?} past the 200 ms timeout"
        );

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(b"Q").unwrap();
        let mut buf = [0u8; 1];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [b'A']);
        server.join().unwrap();
    }

    /// macOS refuses once `listen(1)` has one pending connect. Linux allows a
    /// few more, then returns `EAGAIN` without starting the connect. Either
    /// way the call must come back at once. A blocking `connect` on Linux
    /// waits here until `accept`.
    #[cfg(unix)]
    #[test]
    fn connect_timeout_does_not_spend_its_budget_on_a_full_listen_queue() {
        use std::sync::mpsc;

        use rustix::net::{connect, listen, SocketAddrUnix};

        let path = socket_path("full");
        let _unlink = Unlink(path.clone());
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listen(&listener, 1).unwrap();
        let addr = SocketAddrUnix::new(&path).unwrap();
        let mut held = Vec::new();
        let mut refused = false;
        for _ in 0..64 {
            let sock = super::open_nonblocking_unix().unwrap();
            match connect(&sock, &addr) {
                Ok(()) => held.push(sock),
                Err(rustix::io::Errno::INTR) => {}
                Err(rustix::io::Errno::INPROGRESS) => held.push(sock),
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(
            refused,
            "listen queue still accepted {} connections",
            held.len()
        );

        let target = path.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let result = connect_timeout(&target, Duration::from_secs(2));
            let _ = tx.send((
                started.elapsed(),
                result.map(|_| ()).map_err(|error| error.kind()),
            ));
        });
        let outcome = rx.recv_timeout(Duration::from_secs(3));
        drop(held);
        drop(listener);
        let _ = worker.join();
        let (waited, result) = outcome.expect("connect blocked on a full listen queue");
        assert_ne!(
            result,
            Err(std::io::ErrorKind::TimedOut),
            "a full queue must not consume the connect deadline ({waited:?})"
        );
        assert!(result.is_err(), "a full queue connected after {waited:?}");
        assert!(
            waited < Duration::from_millis(500),
            "full queue waited {waited:?}: {result:?}"
        );
    }

    /// `poll` for write waits out the deadline while the socket stays
    /// unwritable. An in-progress connect uses that wait. An idle socket is
    /// the wrong stimulus: Linux reports it writable before `connect`.
    #[cfg(unix)]
    #[test]
    fn connect_wait_gives_up_when_the_socket_stays_unwritable() {
        use std::sync::mpsc;

        use rustix::io::{ioctl_fionbio, Errno};
        use rustix::net::{socketpair, AddressFamily, SendFlags, SocketFlags, SocketType};

        let (reader, writer) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .unwrap();
        ioctl_fionbio(&writer, true).unwrap();
        let chunk = [0u8; 4096];
        let mut filled = false;
        for _ in 0..4096 {
            match rustix::net::send(&writer, &chunk, SendFlags::empty()) {
                Ok(_) => {}
                Err(Errno::AGAIN) => {
                    filled = true;
                    break;
                }
                Err(err) => panic!("filling the send buffer failed: {err}"),
            }
        }
        assert!(filled, "send buffer never filled");

        let timeout = Duration::from_millis(300);
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let result = super::wait_until_connected(&writer, timeout);
            let _ = tx.send((
                started.elapsed(),
                result.map(|_| ()).map_err(|error| error.kind()),
            ));
        });
        let outcome = rx.recv_timeout(Duration::from_secs(3));
        drop(reader);
        let _ = worker.join();
        let (waited, result) = outcome.expect("connect wait blocked past the timeout");
        assert_eq!(result, Err(std::io::ErrorKind::TimedOut), "{waited:?}");
        assert!(
            waited >= Duration::from_millis(150),
            "wait returned in {waited:?}, before the timeout"
        );
        assert!(
            waited < Duration::from_secs(2),
            "wait took {waited:?} for a 300 ms timeout"
        );
    }
}
