// SPDX-License-Identifier: MIT
use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    sync::mpsc,
    time::Duration,
};

const CHILD: &str = "PRISMATTYC_NATIVE_PROOF_LAUNCH_CHILD";
fn size() -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 640,
        pixel_height: 384,
    }
}
fn command(test: &str, mode: &str) -> (String, Vec<String>, BTreeMap<String, String>) {
    let module = module_path!().split_once("::").unwrap().1;
    let test = format!("{module}::{test}");
    (
        std::env::current_exe().unwrap().to_str().unwrap().into(),
        vec![test, "--exact".into(), "--nocapture".into()],
        BTreeMap::from([(CHILD.into(), mode.into())]),
    )
}

#[test]
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
fn real_pty_launch_inherits_one_endpoint_and_binds_the_same_uid_direct_child() {
    if std::env::var_os(CHILD).is_some() {
        use std::{
            os::fd::{AsRawFd, FromRawFd},
            os::unix::net::UnixStream,
        };
        let raw = std::env::var("PRISMATTYC_NATIVE_FD")
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert_eq!(raw, 3);
        let endpoint = unsafe { UnixStream::from_raw_fd(raw) };
        let flags = unsafe { libc::fcntl(endpoint.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    endpoint.as_raw_fd(),
                    libc::F_SETFD,
                    flags | libc::FD_CLOEXEC,
                )
            },
            0
        );
        #[cfg(target_os = "macos")]
        {
            let mut uid = 0;
            let mut gid = 0;
            assert_eq!(
                unsafe { libc::getpeereid(endpoint.as_raw_fd(), &mut uid, &mut gid) },
                0
            );
            assert_eq!(uid, unsafe { libc::geteuid() });
        }
        println!("NATIVE_LAUNCH_READY");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line, "finish\n");
        return;
    }
    let (program, args, env) = command(
        "real_pty_launch_inherits_one_endpoint_and_binds_the_same_uid_direct_child",
        "native",
    );
    let mut launched = spawn(
        &program,
        &args,
        None,
        &env,
        size(),
        Geometry::new(80, 24, 8, 16),
    )
    .unwrap();
    let bound = launched
        .native
        .as_ref()
        .expect("native binding declined for the real direct child");
    assert_eq!(Some(bound.identity().pid()), launched.session.process_id());
    assert!(bound.identity().birth()[0] > 0);
    assert!(bound.authority.available());
    assert!(bound.authority.scene().is_none());
    finish_after_ready(&mut launched.session, "NATIVE_LAUNCH_READY");
}

#[test]
fn an_ineligible_launch_keeps_ansi_and_does_not_forward_a_native_descriptor() {
    if std::env::var_os(CHILD).is_some() {
        assert!(std::env::var_os("PRISMATTYC_NATIVE_FD").is_none());
        println!("ANSI_LAUNCH_READY");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line, "finish\n");
        return;
    }
    let (program, args, mut env) = command(
        "an_ineligible_launch_keeps_ansi_and_does_not_forward_a_native_descriptor",
        "ansi",
    );
    env.insert("PRISMATTYC_NATIVE_FD".into(), "999".into());
    let mut launched = spawn(
        &program,
        &args,
        None,
        &env,
        size(),
        Geometry::new(1, 1, 8, 16),
    )
    .unwrap();
    assert!(launched.native.is_none());
    finish_after_ready(&mut launched.session, "ANSI_LAUNCH_READY");
}

#[test]
fn kernel_binding_rejects_zero_out_of_range_and_non_child_processes() {
    assert!(matches!(child_identity(0), Err(error) if error.kind()==io::ErrorKind::InvalidInput));
    assert!(
        matches!(child_identity(u32::MAX), Err(error) if error.kind()==io::ErrorKind::InvalidInput)
    );
    assert!(
        matches!(child_identity(std::process::id()), Err(error) if error.kind()==io::ErrorKind::PermissionDenied)
    );
}

fn finish_after_ready(session: &mut PtySession, marker: &'static str) {
    let reader = session.take_reader().unwrap();
    let (sent, received) = mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sent.send(Err("PTY closed before child readiness"));
                    return;
                }
                Ok(_) if line.contains(marker) => {
                    let _ = sent.send(Ok(()));
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = sent.send(Err("PTY failed before child readiness"));
                    return;
                }
            }
        }
    });
    received
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    reading.join().unwrap();
    session
        .take_input_writer()
        .unwrap()
        .write_all(b"finish\n")
        .unwrap();
    assert!(session.wait().unwrap().success());
}
