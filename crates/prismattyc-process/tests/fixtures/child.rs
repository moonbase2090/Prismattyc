// SPDX-License-Identifier: MIT
use std::io::{Read, Write};
use std::process::Stdio;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("plain") => {
            std::io::stdout().write_all(b"OUT\n").unwrap();
            std::io::stderr().write_all(b"ERR\n").unwrap();
            std::process::exit(7);
        }
        Some("echo") => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            std::io::stdout().write_all(b"OUT:").unwrap();
            std::io::stdout().write_all(&input).unwrap();
            std::io::stderr().write_all(b"ERR\n").unwrap();
        }
        Some("inherit") => {
            let mut command = prismattyc_process::capture_command(std::env::current_exe().unwrap());
            command
                .arg("plain")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let output = prismattyc_process::spawn(&mut command)
                .unwrap()
                .wait_with_output()
                .unwrap();
            assert!(output.stdout.is_empty() && output.stderr.is_empty());
            std::io::stdout().write_all(b"EMPTY\n").unwrap();
            std::process::exit(output.status.code().unwrap());
        }
        Some(mode @ ("default_stdin" | "inherit_stdin")) => {
            let mut command = prismattyc_process::capture_command(std::env::current_exe().unwrap());
            command.arg("echo");
            if mode == "inherit_stdin" {
                command.stdin(Stdio::inherit());
            }
            let output = prismattyc_process::spawn(&mut command)
                .unwrap()
                .wait_with_output()
                .unwrap();
            assert!(output.status.success());
            std::io::stdout().write_all(&output.stdout).unwrap();
            std::io::stderr().write_all(&output.stderr).unwrap();
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Some("low_limit") => low_limit(),
        Some("exec") => {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.arg("plain");
            panic!(
                "replacement failed: {}",
                prismattyc_process::exec(&mut command)
            );
        }
        Some("wait") => {
            std::io::stdout().write_all(b"READY\n").unwrap();
            std::io::stdout().flush().unwrap();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();
            assert_eq!(input, "go\n");
            std::io::stdout().write_all(b"DONE\n").unwrap();
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Some("fd") => {
            unsafe extern "C" {
                fn recv(fd: i32, buffer: *mut u8, count: usize, flags: i32) -> isize;
                fn fcntl(fd: i32, operation: i32, ...) -> i32;
            }
            let fd: i32 = args[2].parse().unwrap();
            assert!(fd > 2);
            if unsafe { fcntl(fd, 1) } < 0 {
                assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(9));
                println!("FD_ABSENT");
            } else {
                #[cfg(target_os = "linux")]
                let flags = 0x40;
                #[cfg(target_os = "macos")]
                let flags = 0x80;
                let mut marker = [0; 8];
                let count = unsafe { recv(fd, marker.as_mut_ptr(), marker.len(), flags) };
                assert_eq!(count, 8);
                assert_eq!(&marker, b"AUTH_FD!");
                println!("FD_LEAK");
            }
        }
        other => panic!("unknown fixture mode {other:?}"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn low_limit() {
    use std::mem;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::net::UnixStream;
    let mut limits: libc::rlimit = unsafe { mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
        0
    );
    limits.rlim_cur = 64;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);
    let (writer, reader) = prismattyc_process::fd::socket_pair().unwrap();
    let (source, peer) = prismattyc_process::fd::socket_pair().unwrap();
    let inventory = || {
        (0..64)
            .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
            .collect::<Vec<_>>()
    };
    let before = inventory();
    let mut ancillary = [0usize; 64];
    let byte = b'x';
    let mut vector = libc::iovec {
        iov_base: (&byte as *const u8).cast_mut().cast(),
        iov_len: 1,
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(80 * mem::size_of::<libc::c_int>() as u32) } as _;
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    unsafe {
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(80 * mem::size_of::<libc::c_int>() as u32) as _;
        for index in 0..80 {
            libc::CMSG_DATA(header)
                .cast::<libc::c_int>()
                .add(index)
                .write_unaligned(source.as_raw_fd());
        }
        assert_eq!(libc::sendmsg(writer.as_raw_fd(), &message, 0), 1);
    }
    let error = prismattyc_process::fd::receive(reader.as_fd(), &mut [0; 1], 1).unwrap_err();
    #[cfg(target_os = "macos")]
    assert_eq!(error.raw_os_error(), Some(libc::EMSGSIZE));
    #[cfg(target_os = "linux")]
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<prismattyc_process::fd::AncillaryFailure>(),
        Some(&prismattyc_process::fd::AncillaryFailure::Truncated)
    );
    assert_eq!(inventory(), before);
    drop((source, writer, reader));
    let mut peer = UnixStream::from(peer);
    peer.set_nonblocking(true).unwrap();
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    println!("LOW_FD_CLEAN");
}
