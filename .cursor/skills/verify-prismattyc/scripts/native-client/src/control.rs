// SPDX-License-Identifier: MIT
//! One outstanding operation with one absolute deadline, including fragmented writes/reads.
use std::{
    io,
    net::Shutdown,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    time::{Duration, Instant},
};

pub struct Reply {
    pub bytes: Vec<u8>,
    pub descriptor: Option<OwnedFd>,
}

struct Operation {
    request: Vec<u8>,
    descriptor: Option<OwnedFd>,
    sent: usize,
    response: Vec<u8>,
    received: usize,
    rights: Vec<OwnedFd>,
    expected_opcode: u16,
    expected_bytes: usize,
    deadline: Instant,
    stale_records: usize,
    stale_bytes: usize,
}

pub struct Control {
    socket: UnixStream,
    operation: Option<Operation>,
    closed: bool,
    legacy: bool,
}

impl Control {
    pub fn exchange(
        &mut self,
        request: Vec<u8>,
        descriptor: Option<OwnedFd>,
        deadline: Instant,
    ) -> io::Result<Reply> {
        self.begin(request, descriptor, Instant::now())?;
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.operation = None;
                self.closed = true;
                let _ = self.socket.shutdown(Shutdown::Both);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control exchange deadline",
                ));
            }
            if let Some(reply) = self.step(now)? {
                return Ok(reply);
            }
            let mut descriptor = libc::pollfd {
                fd: self.raw_fd(),
                events: if self.wants_write() {
                    libc::POLLOUT
                } else {
                    libc::POLLIN
                },
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let milliseconds = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    }

    pub fn new(socket: UnixStream) -> io::Result<Self> {
        Self::with_dialect(socket, false)
    }

    pub fn legacy(socket: UnixStream) -> io::Result<Self> {
        Self::with_dialect(socket, true)
    }

    fn with_dialect(socket: UnixStream, legacy: bool) -> io::Result<Self> {
        verify_peer(&socket)?;
        // Darwin sendmsg can wait despite MSG_DONTWAIT. Concurrent endpoint users
        // are unsupported; nonblocking mode also follows the retained shell FD.
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            operation: None,
            closed: false,
            legacy,
        })
    }

    pub fn raw_fd(&self) -> i32 {
        self.socket.as_raw_fd()
    }
    pub fn wants_write(&self) -> bool {
        self.operation
            .as_ref()
            .is_some_and(|op| op.sent < op.request.len())
    }

    pub fn begin(
        &mut self,
        request: Vec<u8>,
        descriptor: Option<OwnedFd>,
        now: Instant,
    ) -> io::Result<()> {
        if self.closed || self.operation.is_some() {
            return Err(crate::invalid("control unavailable"));
        }
        if request.len() < 32
            || request.len() > 4096
            || crate::u32_at(&request, 0) as usize != request.len()
        {
            return Err(crate::invalid("request size"));
        }
        let opcode = crate::u16_at(&request, 4);
        let (expected_opcode, expected_bytes) = match opcode {
            1 if self.legacy => (2, 48),
            1 => (9, 128),
            3 => (4, 40),
            5 => (6, 32),
            7 if self.legacy => (8, 32),
            7 => (15, 48),
            10 if !self.legacy => (11, 48),
            12 if !self.legacy => (13, 48),
            _ => return Err(crate::invalid("request opcode")),
        };
        if descriptor.is_some() != (opcode == 10) {
            return Err(crate::invalid("request descriptors"));
        }
        self.operation = Some(Operation {
            request,
            descriptor,
            sent: 0,
            response: vec![0; 4],
            received: 0,
            rights: Vec::new(),
            expected_opcode,
            expected_bytes,
            deadline: now + Duration::from_millis(200),
            stale_records: 0,
            stale_bytes: 0,
        });
        Ok(())
    }

    pub fn step(&mut self, now: Instant) -> io::Result<Option<Reply>> {
        let result = self.step_inner(now);
        if result.is_err() {
            self.operation = None;
            self.closed = true;
            let _ = self.socket.shutdown(Shutdown::Both);
        }
        result
    }

    fn step_inner(&mut self, now: Instant) -> io::Result<Option<Reply>> {
        let op = self
            .operation
            .as_mut()
            .ok_or_else(|| crate::invalid("no operation"))?;
        if now >= op.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "control operation deadline",
            ));
        }
        if op.sent < op.request.len() {
            let n = match send(&self.socket, &op.request[op.sent..], op.descriptor.as_ref()) {
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "control write"));
            }
            op.sent += n;
            op.descriptor = None;
            if op.sent < op.request.len() {
                return Ok(None);
            }
        }
        // At most the prefix and current record body per step. Never consume a later record.
        for _ in 0..2 {
            let received = match prismattyc_process::fd::receive(
                self.socket.as_fd(),
                &mut op.response[op.received..],
                16,
            ) {
                Ok(received) => received,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            };
            if received.bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control peer closed",
                ));
            }
            op.received += received.bytes;
            op.rights.extend(received.descriptors);
            if op.rights.len() > 16 {
                return Err(crate::invalid("descriptor bound"));
            }
            if op.received != op.response.len() {
                return Ok(None);
            }
            if op.response.len() == 4 {
                let length = crate::u32_at(&op.response, 0) as usize;
                if !(32..=4096).contains(&length) {
                    return Err(crate::invalid("response size"));
                }
                op.response.resize(length, 0);
                continue;
            }
            let r = &op.response;
            let epoch = crate::u64_at(r, 24);
            let wanted_epoch = crate::u64_at(&op.request, 24);
            if epoch < wanted_epoch && crate::u16_at(&op.request, 4) == 1 {
                op.stale_records += 1;
                op.stale_bytes += r.len();
                if op.stale_records > 4 || op.stale_bytes > 16384 {
                    return Err(crate::invalid("stale response bound"));
                }
                validate_reply_shape(r, &op.rights, true, self.legacy)?;
                op.rights.clear();
                op.response = vec![0; 4];
                op.received = 0;
                return Ok(None);
            }
            let opcode = crate::u16_at(r, 4);
            if epoch != wanted_epoch
                || crate::u64_at(r, 16) != crate::u64_at(&op.request, 16)
                || (crate::u16_at(&op.request, 4) != 1
                    && crate::u64_at(r, 8) != crate::u64_at(&op.request, 8))
            {
                return Err(crate::invalid("response identity"));
            }
            validate_reply_shape(r, &op.rights, false, self.legacy)?;
            if opcode == 14 {
                if crate::u64_at(r, 32) <= wanted_epoch
                    || !(1..=4).contains(&crate::u32_at(r, 40))
                    || crate::u32_at(r, 44) != 0
                {
                    return Err(crate::invalid("session-ended body"));
                }
                let op = self.operation.take().unwrap();
                return Ok(Some(Reply {
                    bytes: op.response,
                    descriptor: None,
                }));
            }
            if opcode != op.expected_opcode || r.len() != op.expected_bytes {
                return Err(crate::invalid("unexpected response"));
            }
            let op = self.operation.take().unwrap();
            return Ok(Some(Reply {
                bytes: op.response,
                descriptor: op.rights.into_iter().next(),
            }));
        }
        Ok(None)
    }
}

fn validate_reply_shape(
    bytes: &[u8],
    rights: &[OwnedFd],
    stale: bool,
    legacy: bool,
) -> io::Result<()> {
    let opcode = crate::u16_at(bytes, 4);
    let expected = match opcode {
        2 if stale || legacy => 48,
        8 if stale || legacy => 32,
        9 => 128,
        4 => 40,
        6 => 32,
        11 | 13 | 14 | 15 => 48,
        _ => return Err(crate::invalid("reply opcode")),
    };
    if bytes.len() != expected
        || rights.len() != usize::from(opcode == 9 || opcode == 2)
        || crate::u64_at(bytes, 8) == 0
    {
        return Err(crate::invalid("reply layout or descriptors"));
    }
    let flags = crate::u16_at(bytes, 6);
    let active_flags = if legacy { 0 } else { 2 };
    if (opcode == 4
        && if stale {
            flags > 3
        } else {
            flags != active_flags
        })
        || (opcode != 4 && flags != 0)
    {
        return Err(crate::invalid("reply flags"));
    }
    Ok(())
}

fn send(socket: &UnixStream, bytes: &[u8], descriptor: Option<&OwnedFd>) -> io::Result<usize> {
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    let mut ancillary = [0usize; 4];
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    if let Some(descriptor) = descriptor {
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(4) as _;
            libc::CMSG_DATA(header)
                .cast::<i32>()
                .write_unaligned(descriptor.as_raw_fd());
        }
    }
    let n = unsafe {
        libc::sendmsg(
            socket.as_raw_fd(),
            &message,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn verify_peer(socket: &UnixStream) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let uid = {
        let (mut uid, mut gid) = (0, 0);
        if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    #[cfg(target_os = "linux")]
    let uid = {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != std::mem::size_of_val(&credentials) || credentials.pid <= 0 {
            return Err(crate::invalid("peer credentials"));
        }
        credentials.uid
    };
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "peer UID"));
    }
    Ok(())
}
