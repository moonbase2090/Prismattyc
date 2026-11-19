// SPDX-License-Identifier: MIT
use super::*;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    root: PathBuf,
    executable: PathBuf,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "process-boundary-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let executable = root.join(format!("process-child{}", std::env::consts::EXE_SUFFIX));
        let fixture = Self {
            root,
            executable,
            _serial: serial,
        };
        let dependency = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
        std::fs::write(fixture.root.join("Cargo.toml"), format!("[package]\nname=\"process-child\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[workspace]\n[dependencies]\nlibc=\"0.2\"\nprismattyc-process={{path={dependency:?}}}\n[[bin]]\nname=\"process-child\"\npath=\"child.rs\"\n")).unwrap();
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/child.rs"),
            fixture.root.join("child.rs"),
        )
        .unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"),
            fixture.root.join("Cargo.lock"),
        )
        .unwrap();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = capture_command(cargo);
        command
            .args(["build", "--offline", "--manifest-path"])
            .arg(fixture.root.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(fixture.root.join("target"));
        let built = spawn(&mut command).unwrap().wait_with_output().unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        std::fs::copy(
            fixture
                .root
                .join("target/debug")
                .join(format!("process-child{}", std::env::consts::EXE_SUFFIX)),
            &fixture.executable,
        )
        .unwrap();
        fixture
    }
    fn command(&self, mode: &str) -> Command {
        let mut command = capture_command(&self.executable);
        command.arg(mode);
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn capture_defaults_collect_both_streams_and_exit_status() {
    let fixture = Fixture::new();
    let output = spawn(&mut fixture.command("plain"))
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"OUT\n");
    assert_eq!(output.stderr, b"ERR\n");
}

#[test]
fn capture_preserves_explicit_input_and_file_output() {
    let fixture = Fixture::new();
    let path = fixture.root.join("stdout");
    let file = std::fs::File::create(&path).unwrap();
    let mut command = fixture.command("echo");
    command.stdin(Stdio::piped()).stdout(file);
    let mut child = spawn(&mut command).unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"ERR\n");
    assert_eq!(std::fs::read(path).unwrap(), b"OUT:hello\n");
}

#[test]
fn capture_preserves_inherited_streams() {
    let fixture = Fixture::new();
    let output = spawn(&mut fixture.command("inherit"))
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"OUT\nEMPTY\n");
    assert_eq!(output.stderr, b"ERR\n");
}

#[test]
fn replacement_preserves_streams_and_exit_status() {
    let fixture = Fixture::new();
    let output = spawn(&mut fixture.command("exec"))
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"OUT\n");
    assert_eq!(output.stderr, b"ERR\n");
}

#[test]
fn capture_defaults_close_stdin_but_preserve_explicit_inheritance() {
    let fixture = Fixture::new();
    let input = fixture.root.join("input");
    std::fs::write(&input, b"hello\n").unwrap();
    for (mode, expected) in [
        ("default_stdin", b"OUT:".as_slice()),
        ("inherit_stdin", b"OUT:hello\n".as_slice()),
    ] {
        let mut command = fixture.command(mode);
        command.stdin(std::fs::File::open(&input).unwrap());
        let output = spawn(&mut command).unwrap().wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, expected);
        assert_eq!(output.stderr, b"ERR\n");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod descriptor {
    use super::*;
    use std::io::Read;
    use std::mem;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    fn send(control: BorrowedFd<'_>, descriptors: &[BorrowedFd<'_>]) {
        let payload = descriptors.len() * mem::size_of::<libc::c_int>();
        let capacity = unsafe { libc::CMSG_SPACE(payload as _) as usize };
        let mut ancillary = vec![0usize; capacity.div_ceil(mem::size_of::<usize>())];
        let byte = b'x';
        let mut vector = libc::iovec {
            iov_base: (&byte as *const u8).cast_mut().cast(),
            iov_len: 1,
        };
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = capacity as _;
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(payload as _) as _;
            for (index, descriptor) in descriptors.iter().enumerate() {
                libc::CMSG_DATA(header)
                    .cast::<libc::c_int>()
                    .add(index)
                    .write_unaligned(descriptor.as_raw_fd());
            }
            assert_eq!(libc::sendmsg(control.as_raw_fd(), &message, 0), 1);
        }
    }

    fn eof(fd: OwnedFd) {
        let mut peer = UnixStream::from(fd);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap(),
            0,
            "rejected descriptor still has an open writer"
        );
    }

    #[test]
    fn both_created_endpoints_are_close_on_exec() {
        let _serial = SERIAL.lock().unwrap();
        let (one, two) = fd::socket_pair().unwrap();
        for endpoint in [one, two] {
            let flags = unsafe { libc::fcntl(endpoint.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        }
    }

    #[test]
    fn fd_forbidding_receives_still_close_unexpected_rights() {
        let _serial = SERIAL.lock().unwrap();
        let (writer, reader) = fd::socket_pair().unwrap();
        let (source, peer) = fd::socket_pair().unwrap();
        send(writer.as_fd(), &[source.as_fd()]);
        // Keep an external socket reference until rights leave the transit
        // queue; Darwin's Unix-domain collector can otherwise flush it.
        let error = fd::receive(reader.as_fd(), &mut [0; 1], 0).unwrap_err();
        drop(source);
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<fd::AncillaryFailure>(),
            Some(&fd::AncillaryFailure::DescriptorCount {
                received: 1,
                maximum: 0
            })
        );
        eof(peer);
    }

    #[test]
    fn transferred_descriptor_is_functional_and_close_on_exec() {
        let _serial = SERIAL.lock().unwrap();
        let (writer, reader) = fd::socket_pair().unwrap();
        let (source, peer) = fd::socket_pair().unwrap();
        send(writer.as_fd(), &[source.as_fd()]);
        let mut bytes = [0; 1];
        let mut received = fd::receive(reader.as_fd(), &mut bytes, 1).unwrap();
        assert_eq!(received.bytes, 1);
        assert_eq!(bytes, *b"x");
        assert_eq!(received.descriptors.len(), 1);
        let descriptor = received.descriptors.pop().unwrap();
        assert_eq!(
            unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC
        );
        let mut delivered = UnixStream::from(descriptor);
        let mut peer = UnixStream::from(peer);
        delivered.write_all(b"wire").unwrap();
        let mut payload = [0; 4];
        peer.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"wire");
    }

    #[test]
    fn excess_descriptors_are_all_closed_before_rejection_returns() {
        let _serial = SERIAL.lock().unwrap();
        let (writer, reader) = fd::socket_pair().unwrap();
        let (one, one_peer) = fd::socket_pair().unwrap();
        let (two, two_peer) = fd::socket_pair().unwrap();
        send(writer.as_fd(), &[one.as_fd(), two.as_fd()]);
        let error = fd::receive(reader.as_fd(), &mut [0; 1], 1).unwrap_err();
        drop((one, two));
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        eof(one_peer);
        eof(two_peer);
    }

    #[cfg(target_os = "macos")]
    #[derive(Clone, Copy)]
    enum WindowKind {
        Creation,
        Reception,
    }

    #[cfg(target_os = "macos")]
    fn assert_window_excludes_spawn(kind: WindowKind) {
        use crate::test_hooks;
        use std::sync::mpsc;
        use std::time::Duration;
        #[derive(Debug)]
        enum Event {
            Window(i32),
            Gate(bool),
            Created,
            Completed(io::Result<std::process::Output>),
        }
        let fixture = Fixture::new();
        let mut keepalive = Vec::new();
        let operation: Box<dyn FnOnce() -> io::Result<Vec<OwnedFd>> + Send> = match kind {
            WindowKind::Creation => Box::new(|| {
                let (one, two) = fd::socket_pair()?;
                Ok(vec![one, two])
            }),
            WindowKind::Reception => {
                let (writer, reader) = fd::socket_pair().unwrap();
                let (source, peer) = fd::socket_pair().unwrap();
                assert_eq!(
                    unsafe { libc::send(peer.as_raw_fd(), b"AUTH_FD!".as_ptr().cast(), 8, 0) },
                    8
                );
                send(writer.as_fd(), &[source.as_fd()]);
                keepalive.extend([writer, source, peer]);
                Box::new(move || {
                    let mut bytes = [0; 1];
                    let result = fd::receive(reader.as_fd(), &mut bytes, 1)?;
                    assert_eq!(bytes, *b"x");
                    Ok(result.descriptors)
                })
            }
        };
        let (events, received) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let window_events = events.clone();
        let created_events = events.clone();
        let producer = std::thread::spawn(move || {
            test_hooks::on_window(move |descriptors| {
                let target = match kind {
                    WindowKind::Creation => {
                        assert_eq!(descriptors.len(), 2);
                        assert_eq!(
                            unsafe {
                                libc::send(descriptors[0], b"AUTH_FD!".as_ptr().cast(), 8, 0)
                            },
                            8
                        );
                        descriptors[1]
                    }
                    WindowKind::Reception => {
                        assert_eq!(descriptors.len(), 1);
                        descriptors[0]
                    }
                };
                window_events.send(Event::Window(target)).unwrap();
                resumed.recv().unwrap();
            });
            let result = operation();
            created_events.send(Event::Created).unwrap();
            result
        });
        let first = received.recv_timeout(Duration::from_secs(30));
        let descriptor = match first {
            Ok(Event::Window(descriptor)) => descriptor,
            other => {
                let _ = resume.send(());
                let _ = producer.join();
                panic!("FD window was not reached: {other:?}");
            }
        };
        let mut command = fixture.command("fd");
        command.arg(descriptor.to_string());
        let gate_events = events.clone();
        let complete_events = events.clone();
        let launcher = std::thread::spawn(move || {
            test_hooks::on_acquire(move |blocked| gate_events.send(Event::Gate(blocked)).unwrap());
            let output = spawn(&mut command).and_then(std::process::Child::wait_with_output);
            complete_events.send(Event::Completed(output)).unwrap();
        });
        let observed = received.recv_timeout(Duration::from_secs(30));
        let description = format!("{observed:?}");
        let (blocked, mut completed) = match observed {
            Ok(Event::Gate(blocked)) => (blocked, None),
            Ok(Event::Completed(result)) => (false, Some(result)),
            _ => (false, None),
        };
        let _ = resume.send(());
        let produced = producer.join();
        while completed.is_none() {
            if let Event::Completed(result) = received
                .recv_timeout(Duration::from_secs(30))
                .expect("child completion")
            {
                completed = Some(result);
            }
        }
        launcher.join().unwrap();
        let descriptors = produced.unwrap().unwrap();
        let output = completed.unwrap().unwrap();
        drop(descriptors);
        drop(keepalive);
        assert!(
            blocked,
            "launch crossed the descriptor window: {description}; child={output:?}"
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"FD_ABSENT\n");
        assert_eq!(output.stderr, b"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn creation_window_excludes_child_launch() {
        assert_window_excludes_spawn(WindowKind::Creation);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn receive_window_excludes_child_launch() {
        assert_window_excludes_spawn(WindowKind::Reception);
    }

    #[test]
    fn oversized_rights_close_every_installed_descriptor() {
        let _serial = SERIAL.lock().unwrap();
        let (writer, reader) = fd::socket_pair().unwrap();
        let (source, peer) = fd::socket_pair().unwrap();
        send(writer.as_fd(), &vec![source.as_fd(); 80]);
        let error = fd::receive(reader.as_fd(), &mut [0; 1], 16).unwrap_err();
        drop(source);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        eof(peer);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn fd_limit_exhaustion_installs_no_unowned_descriptors() {
    let fixture = Fixture::new();
    let output = spawn(&mut fixture.command("low_limit"))
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"LOW_FD_CLEAN\n");
    assert_eq!(output.stderr, b"");
}

#[cfg(target_os = "macos")]
#[test]
fn waiting_for_exit_releases_the_launch_gate() {
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    let fixture = Fixture::new();
    let (mut input, child_input) = UnixStream::pair().unwrap();
    let (output, child_output) = UnixStream::pair().unwrap();
    let mut command = fixture.command("wait");
    command
        .stdin(OwnedFd::from(child_input))
        .stdout(OwnedFd::from(child_output));
    let waiting = std::thread::spawn(move || status(&mut command));
    let mut output = BufReader::new(output);
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready, "READY\n");

    let (events, received) = mpsc::channel();
    let mut second = fixture.command("plain");
    let launching = std::thread::spawn(move || {
        test_hooks::on_acquire(move |blocked| events.send(blocked).unwrap());
        spawn(&mut second).unwrap().wait_with_output().unwrap()
    });
    let observed = received.recv_timeout(Duration::from_secs(30));
    input.write_all(b"go\n").unwrap();
    drop(input);
    let status = waiting.join().unwrap().unwrap();
    let second = launching.join().unwrap();
    let mut finished = String::new();
    output.read_to_string(&mut finished).unwrap();
    assert!(status.success());
    assert_eq!(finished, "DONE\n");
    assert_eq!(second.status.code(), Some(7));
    assert_eq!(second.stdout, b"OUT\n");
    assert_eq!(second.stderr, b"ERR\n");
    assert!(!observed.unwrap(), "wait held the launch gate");
}
