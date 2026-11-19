// SPDX-License-Identifier: MIT
//! Bounded control reception; every stream byte passes through the FD boundary.

use super::{
    legacy,
    profile::Dialect,
    wire12::{self, Record, Reply, ReplyIdentity, MAX_RECORD_BYTES},
};
use prismattyc_process::fd;
#[cfg(target_os = "macos")]
use prismattyc_process::fd::AncillaryFailure;
use std::{
    io,
    net::Shutdown,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const RECORD_DEADLINE: Duration = Duration::from_millis(200);

/// Shared by every endpoint in the host process, independent of PTY epochs.
pub static AVAILABILITY: NativeAvailability = NativeAvailability::new();

pub struct NativeAvailability {
    disabled: AtomicBool,
    truncations: AtomicU64,
}

impl NativeAvailability {
    const fn new() -> Self {
        Self {
            disabled: AtomicBool::new(false),
            truncations: AtomicU64::new(0),
        }
    }

    pub fn available(&self) -> bool {
        !self.disabled.load(Ordering::Acquire)
    }

    pub fn truncations(&self) -> u64 {
        self.truncations.load(Ordering::Relaxed)
    }

    fn observe(&self, error: &io::Error) {
        #[cfg(target_os = "macos")]
        if matches!(
            error
                .get_ref()
                .and_then(|error| error.downcast_ref::<AncillaryFailure>()),
            Some(AncillaryFailure::Truncated)
        ) {
            self.disabled.store(true, Ordering::Release);
            let _ = self
                .truncations
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(1))
                });
        }
        #[cfg(not(target_os = "macos"))]
        let _ = error;
    }
}

#[derive(Debug)]
pub struct OwnedRequest {
    pub record: Record,
    pub descriptor: Option<OwnedFd>,
    pub started: Instant,
    pub hello_dialect: Option<Dialect>,
}

#[derive(Debug)]
pub enum Receive {
    Pending,
    Request(OwnedRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flush {
    Complete,
    Pending,
}

struct Outgoing {
    bytes: Vec<u8>,
    written: usize,
    descriptor: Option<OwnedFd>,
    deadline: Instant,
}

pub struct ControlEndpoint<'a> {
    socket: Option<Arc<UnixStream>>,
    availability: &'a NativeAvailability,
    bytes: [u8; MAX_RECORD_BYTES],
    filled: usize,
    length: Option<usize>,
    descriptors: Vec<OwnedFd>,
    started: Option<Instant>,
    outgoing: Option<Outgoing>,
}

impl ControlEndpoint<'static> {
    pub fn new(socket: UnixStream) -> io::Result<Self> {
        Self::with_availability(socket, &AVAILABILITY)
    }
}

impl<'a> ControlEndpoint<'a> {
    fn with_availability(
        socket: UnixStream,
        availability: &'a NativeAvailability,
    ) -> io::Result<Self> {
        if !availability.available() {
            socket.shutdown(Shutdown::Both)?;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native transport disabled",
            ));
        }
        verify_peer(&socket)?;
        // Darwin send can wait for space despite MSG_DONTWAIT. This is the
        // host's distinct endpoint; its file status does not affect the peer.
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket: Some(Arc::new(socket)),
            availability,
            bytes: [0; MAX_RECORD_BYTES],
            filled: 0,
            length: None,
            descriptors: Vec::new(),
            started: None,
            outgoing: None,
        })
    }

    pub fn watch(&self, wake: super::readiness::Wake) -> io::Result<super::readiness::Readiness> {
        let socket = self.socket.as_ref().ok_or(io::ErrorKind::NotConnected)?;
        super::readiness::Readiness::new(Arc::clone(socket), wake)
    }

    pub fn deadline(&self) -> Option<Instant> {
        let receive = self.started.map(|started| started + RECORD_DEADLINE);
        let send = self.outgoing.as_ref().map(|reply| reply.deadline);
        match (receive, send) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    /// At most two recvmsg calls, always prefix then current-record remainder.
    /// Callers remove an installed scene before handling an error as ANSI fallback.
    pub fn poll(&mut self, now: Instant) -> io::Result<Receive> {
        if self.socket.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "native endpoint closed",
            ));
        }
        if !self.availability.available() {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::Unsupported,
                "native transport disabled",
            )));
        }
        if self.outgoing.is_some() {
            return Ok(Receive::Pending);
        }
        if self
            .started
            .is_some_and(|started| now.saturating_duration_since(started) >= RECORD_DEADLINE)
        {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::TimedOut,
                "partial native record",
            )));
        }
        for _ in 0..2 {
            let end = self.length.unwrap_or(4);
            let socket = self.socket.as_ref().unwrap();
            let received = match fd::receive(socket.as_fd(), &mut self.bytes[self.filled..end], 16)
            {
                Ok(received) => received,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(Receive::Pending);
                }
                Err(error) => return Err(self.fail(error)),
            };
            self.descriptors.extend(received.descriptors);
            if self.descriptors.len() > 1 {
                return Err(self.fail(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "extra native descriptors",
                )));
            }
            if received.bytes == 0 {
                return Err(self.fail(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "native peer closed",
                )));
            }
            self.started.get_or_insert(now);
            self.filled += received.bytes;
            if self.length.is_none() && self.filled == 4 {
                let length = u32::from_le_bytes(self.bytes[..4].try_into().unwrap()) as usize;
                if !(wire12::HEADER_BYTES..=MAX_RECORD_BYTES).contains(&length) {
                    return Err(self.fail(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "native record length",
                    )));
                }
                self.length = Some(length);
            }
            if self.length == Some(self.filled) {
                let legacy_hello =
                    self.filled == 56 && self.bytes[4..6] == [1, 0] && self.bytes[38..40] == [0, 0];
                let decoded = if legacy_hello {
                    legacy::decode_hello(&self.bytes[..self.filled])
                        .map_err(|error| format!("legacy native record: {error:?}"))
                } else {
                    wire12::decode(&self.bytes[..self.filled])
                        .map_err(|error| format!("native record: {error:?}"))
                };
                let record = match decoded {
                    Ok(record) => record,
                    Err(error) => {
                        return Err(self.fail(io::Error::new(io::ErrorKind::InvalidData, error)))
                    }
                };
                if self.descriptors.len() != record.request.descriptor_count() {
                    return Err(self.fail(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "native descriptor count",
                    )));
                }
                let descriptor = self.descriptors.pop();
                self.filled = 0;
                self.length = None;
                let started = self.started.take().unwrap();
                let hello_dialect = matches!(record.request, wire12::Request::Hello { .. })
                    .then_some(if legacy_hello {
                        Dialect::Grid0
                    } else {
                        Dialect::Images2
                    });
                return Ok(Receive::Request(OwnedRequest {
                    record,
                    descriptor,
                    started,
                    hello_dialect,
                }));
            }
        }
        Ok(Receive::Pending)
    }

    pub fn queue_reply(
        &mut self,
        identity: ReplyIdentity,
        reply: Reply,
        descriptor: Option<OwnedFd>,
        request_started: Instant,
    ) -> io::Result<()> {
        self.queue_encoded(
            reply,
            wire12::encode(identity, reply),
            descriptor,
            request_started,
        )
    }

    pub fn queue_legacy_reply(
        &mut self,
        identity: ReplyIdentity,
        reply: Reply,
        descriptor: Option<OwnedFd>,
        request_started: Instant,
    ) -> io::Result<()> {
        let bytes = legacy::encode(identity, reply).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("legacy native reply: {error:?}"),
            )
        })?;
        self.queue_encoded(reply, bytes, descriptor, request_started)
    }

    fn queue_encoded(
        &mut self,
        reply: Reply,
        bytes: Vec<u8>,
        descriptor: Option<OwnedFd>,
        request_started: Instant,
    ) -> io::Result<()> {
        if self.socket.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "native endpoint closed",
            ));
        }
        if self.outgoing.is_some() || self.filled != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native operation outstanding",
            ));
        }
        if matches!(reply, Reply::Setup { .. }) != descriptor.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native reply descriptor count",
            ));
        }
        let budget = if matches!(reply, Reply::Pong(_)) {
            Duration::from_millis(100)
        } else {
            RECORD_DEADLINE
        };
        self.outgoing = Some(Outgoing {
            bytes,
            written: 0,
            descriptor,
            deadline: request_started + budget,
        });
        Ok(())
    }

    /// Revocation may discard only a reply whose bytes have not entered the stream.
    pub fn discard_reply(&mut self) -> io::Result<()> {
        if self
            .outgoing
            .as_ref()
            .is_some_and(|reply| reply.written != 0)
        {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native revocation during a partial reply",
            )));
        }
        self.outgoing = None;
        Ok(())
    }

    /// One nonblocking sendmsg. Rights accompany only the first successful bytes.
    pub fn flush(&mut self, now: Instant) -> io::Result<Flush> {
        let Some(socket) = self.socket.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "native endpoint closed",
            ));
        };
        if !self.availability.available() {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::Unsupported,
                "native transport disabled",
            )));
        }
        let Some(outgoing) = self.outgoing.as_mut() else {
            return Ok(Flush::Complete);
        };
        if now >= outgoing.deadline {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::TimedOut,
                "native reply deadline",
            )));
        }
        let remaining = &outgoing.bytes[outgoing.written..];
        let mut vector = libc::iovec {
            iov_base: remaining.as_ptr().cast_mut().cast(),
            iov_len: remaining.len(),
        };
        let mut ancillary = [0usize; 4];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        if let Some(descriptor) = &outgoing.descriptor {
            message.msg_control = ancillary.as_mut_ptr().cast();
            message.msg_controllen =
                unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as _;
            // The aligned ancillary buffer holds one complete SCM_RIGHTS value.
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
                libc::CMSG_DATA(header)
                    .cast::<libc::c_int>()
                    .write_unaligned(descriptor.as_raw_fd());
            }
        }
        let sent = unsafe {
            libc::sendmsg(
                socket.as_raw_fd(),
                &message,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if sent < 0 {
            let error = io::Error::last_os_error();
            return if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                Ok(Flush::Pending)
            } else {
                Err(self.fail(error))
            };
        }
        if sent == 0 || sent as usize > remaining.len() {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::WriteZero,
                "native reply write",
            )));
        }
        outgoing.written += sent as usize;
        outgoing.descriptor.take();
        if outgoing.written == outgoing.bytes.len() {
            self.outgoing = None;
            Ok(Flush::Complete)
        } else {
            Ok(Flush::Pending)
        }
    }

    fn fail(&mut self, error: io::Error) -> io::Error {
        self.availability.observe(&error);
        self.close();
        error
    }

    pub fn close(&mut self) {
        if let Some(socket) = self.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.descriptors.clear();
        self.outgoing = None;
        self.filled = 0;
        self.length = None;
        self.started = None;
    }
}

impl Drop for ControlEndpoint<'_> {
    fn drop(&mut self) {
        self.close();
    }
}

fn verify_peer(socket: &UnixStream) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let uid = {
        let mut uid = 0;
        let mut gid = 0;
        // getpeereid writes precisely these two scalar outputs.
        if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    #[cfg(target_os = "linux")]
    let uid = {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
        // The initialized output has exactly the requested ucred capacity.
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
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native peer credentials",
            ));
        }
        credentials.uid
    };
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "native peer uid",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::wire12::literals;
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::BorrowedFd,
    };

    fn reply_identity(sequence: u64) -> ReplyIdentity {
        ReplyIdentity {
            session: wire12::SessionId::new(1).unwrap(),
            sequence: wire12::RequestSequence::new(sequence).unwrap(),
            epoch: wire12::PtyEpoch::new(7).unwrap(),
        }
    }

    fn fill_send_buffer(endpoint: &ControlEndpoint<'_>) -> usize {
        let socket = endpoint.socket.as_ref().unwrap();
        let capacity: libc::c_int = 8192;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&capacity as *const libc::c_int).cast(),
                    4,
                )
            },
            0
        );
        let filler = [42u8; 1];
        let mut total = 0;
        for _ in 0..65_536 {
            let sent = unsafe {
                libc::send(
                    socket.as_raw_fd(),
                    filler.as_ptr().cast(),
                    filler.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if sent < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                return total;
            }
            assert!(sent > 0);
            total += sent as usize;
        }
        panic!("configured socket never signaled backpressure");
    }

    fn pair() -> (UnixStream, UnixStream) {
        let (host, client) = fd::socket_pair().unwrap();
        (host.into(), client.into())
    }
    pub(crate) fn send_rights(socket: &UnixStream, bytes: &[u8], rights: &[BorrowedFd<'_>]) {
        let mut vector = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut ancillary = [0usize; 16];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        let length = rights.len() * std::mem::size_of::<libc::c_int>();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(length as _) } as _;
        #[cfg(target_os = "macos")]
        let capacity = std::mem::size_of_val(&ancillary) as libc::socklen_t;
        #[cfg(target_os = "linux")]
        let capacity = std::mem::size_of_val(&ancillary);
        assert!(message.msg_controllen <= capacity);
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(length as _) as _;
            for (index, right) in rights.iter().enumerate() {
                libc::CMSG_DATA(header)
                    .cast::<libc::c_int>()
                    .add(index)
                    .write_unaligned(right.as_raw_fd());
            }
            assert_eq!(
                libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
                bytes.len() as isize
            );
        }
    }
    fn request(received: Receive) -> OwnedRequest {
        match received {
            Receive::Request(request) => request,
            Receive::Pending => panic!("complete queued request was not returned"),
        }
    }

    #[test]
    fn fragmented_records_keep_the_first_byte_time_for_reply_deadlines() {
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let now = Instant::now();
        client.write_all(&literals::PING_IDLE[..4]).unwrap();
        assert!(matches!(endpoint.poll(now).unwrap(), Receive::Pending));
        client.write_all(&literals::PING_IDLE[4..]).unwrap();
        let request = request(endpoint.poll(now + Duration::from_millis(99)).unwrap());
        assert_eq!(request.started, now);
        endpoint
            .queue_reply(
                reply_identity(3),
                Reply::Pong(wire12::UploadState::Idle),
                None,
                request.started,
            )
            .unwrap();
        assert_eq!(
            endpoint
                .flush(now + Duration::from_millis(100))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eof(client);
    }

    #[test]
    fn revoking_an_unwritten_reply_preserves_the_endpoint_for_the_new_epoch() {
        let availability = NativeAvailability::new();
        let (host, client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let now = Instant::now();
        endpoint
            .queue_reply(
                reply_identity(3),
                Reply::Pong(wire12::UploadState::Idle),
                None,
                now,
            )
            .unwrap();
        endpoint.discard_reply().unwrap();
        assert_eq!(endpoint.flush(now).unwrap(), Flush::Complete);
        endpoint
            .queue_reply(reply_identity(4), Reply::Released, None, now)
            .unwrap();
        assert_eq!(endpoint.flush(now).unwrap(), Flush::Complete);
        let mut bytes = [0; 32];
        assert_eq!(
            fd::receive(client.as_fd(), &mut bytes, 0).unwrap().bytes,
            32
        );
        assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), 6);
        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 4);
    }

    #[test]
    fn revoking_a_partially_written_record_closes_the_endpoint_instead_of_splicing_records() {
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        endpoint
            .queue_reply(
                reply_identity(3),
                Reply::Pong(wire12::UploadState::Idle),
                None,
                Instant::now(),
            )
            .unwrap();
        // Arrange a real one-byte prefix, then record its completed write. The
        // platform need not happen to split a small sendmsg at this boundary.
        let first = endpoint.outgoing.as_ref().unwrap().bytes[0];
        endpoint
            .socket
            .as_ref()
            .unwrap()
            .as_ref()
            .write_all(&[first])
            .unwrap();
        endpoint.outgoing.as_mut().unwrap().written = 1;
        assert_eq!(
            endpoint.discard_reply().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        let mut bytes = [0];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [48]);
        assert_eof(client);
        assert_eq!(
            endpoint.poll(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }
    fn assert_eof(mut socket: UnixStream) {
        socket.set_nonblocking(true).unwrap();
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn fragmented_prefix_and_late_rights_preserve_the_record_and_cloexec() {
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let now = Instant::now();
        assert!(matches!(endpoint.poll(now).unwrap(), Receive::Pending));
        client.write_all(&literals::PUT_IMAGE[..2]).unwrap();
        assert!(matches!(endpoint.poll(now).unwrap(), Receive::Pending));
        client.write_all(&literals::PUT_IMAGE[2..4]).unwrap();
        assert!(matches!(endpoint.poll(now).unwrap(), Receive::Pending));
        let (source, mut source_peer) = pair();
        send_rights(&client, &literals::PUT_IMAGE[4..], &[source.as_fd()]);
        let result = request(endpoint.poll(now).unwrap());
        drop(source);
        assert_eq!(result.record, wire12::decode(literals::PUT_IMAGE).unwrap());
        let descriptor = result.descriptor.unwrap();
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        source_peer.write_all(b"x").unwrap();
        let mut source: UnixStream = descriptor.into();
        let mut byte = [0];
        source.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
        assert!(availability.available());
        assert_eq!(availability.truncations(), 0);
    }

    #[test]
    fn queued_requests_stay_separate_and_release_received_rights() {
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let (source, source_peer) = pair();
        send_rights(&client, literals::PUT_IMAGE, &[source.as_fd()]);
        client.write_all(literals::PING_IDLE).unwrap();
        let now = Instant::now();
        let first = request(endpoint.poll(now).unwrap());
        drop(source);
        assert!(matches!(first.record.request, wire12::Request::PutImage(_)));
        drop(first);
        assert_eof(source_peer);
        let next = request(endpoint.poll(now).unwrap());
        assert_eq!(next.record, wire12::decode(literals::PING_IDLE).unwrap());
        assert!(next.descriptor.is_none());
    }

    #[test]
    fn forbidden_and_missing_rights_close_the_real_endpoint() {
        for bytes in [literals::HELLO, literals::PING_IDLE, literals::RETIRE_IMAGE] {
            let availability = NativeAvailability::new();
            let (host, client) = pair();
            let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
            let (source, source_peer) = pair();
            send_rights(&client, bytes, &[source.as_fd()]);
            assert_eq!(
                endpoint.poll(Instant::now()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            drop(source);
            assert_eof(client);
            assert_eof(source_peer);
            assert!(availability.available());
        }
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        client.write_all(literals::PUT_IMAGE).unwrap();
        assert_eq!(
            endpoint.poll(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eof(client);
    }

    #[test]
    fn rights_on_separate_fragments_are_all_owned_and_closed_on_rejection() {
        let availability = NativeAvailability::new();
        let (host, client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let (first, first_peer) = pair();
        send_rights(&client, &literals::PUT_IMAGE[..4], &[first.as_fd()]);
        assert!(matches!(
            endpoint.poll(Instant::now()).unwrap(),
            Receive::Pending
        ));
        drop(first);
        let (second, second_peer) = pair();
        send_rights(&client, &literals::PUT_IMAGE[4..], &[second.as_fd()]);
        assert_eq!(
            endpoint.poll(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        drop(second);
        assert_eof(client);
        assert_eof(first_peer);
        assert_eof(second_peer);
    }

    #[test]
    fn partial_record_deadline_uses_elapsed_time_without_sleeping() {
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let now = Instant::now();
        client.write_all(&literals::HELLO[..4]).unwrap();
        assert!(matches!(endpoint.poll(now).unwrap(), Receive::Pending));
        assert_eq!(
            endpoint
                .poll(now + Duration::from_millis(200))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eof(client);
        assert_eq!(
            endpoint.poll(now).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn descriptor_exhaustion_errors_close_without_partial_setup() {
        for errno in [libc::EMFILE, libc::EMSGSIZE] {
            let availability = NativeAvailability::new();
            let (host, client) = pair();
            let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
            let returned = endpoint.fail(io::Error::from_raw_os_error(errno));
            assert_eq!(returned.raw_os_error(), Some(errno));
            assert_eof(client);
            assert_eq!(
                endpoint.poll(Instant::now()).unwrap_err().kind(),
                io::ErrorKind::NotConnected
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn darwin_truncation_closes_once_and_latches_across_new_endpoints() {
        let availability = NativeAvailability::new();
        let (host, client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        endpoint.fail(io::Error::new(
            io::ErrorKind::InvalidData,
            AncillaryFailure::Truncated,
        ));
        assert_eof(client);
        assert!(!availability.available());
        assert_eq!(availability.truncations(), 1);
        assert_eq!(
            endpoint.poll(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(availability.truncations(), 1);
        let (next_host, next_client) = pair();
        assert!(ControlEndpoint::with_availability(next_host, &availability).is_err());
        assert_eof(next_client);
        assert_eq!(availability.truncations(), 1);
    }

    #[test]
    fn setup_sends_one_initialized_ring_descriptor_and_exact_reply_bytes() {
        use super::super::ring::{RingContext, SharedRing};
        let availability = NativeAvailability::new();
        let (host, client) = pair();
        #[cfg(target_os = "linux")]
        let mut client = client;
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let ring = SharedRing::create(RingContext {
            session: wire12::SessionId::new(1).unwrap(),
            epoch: wire12::PtyEpoch::new(7).unwrap(),
            metrics: wire12::MetricsEpoch::new(9).unwrap(),
            columns: 1,
            rows: 1,
        })
        .unwrap();
        let kind = if cfg!(target_os = "macos") {
            wire12::BackingKind::DarwinFixedPosix
        } else {
            wire12::BackingKind::LinuxSealed
        };
        let reply = Reply::Setup {
            ring: wire12::RingExtent::new(kind, ring.physical_bytes()).unwrap(),
            metrics: wire12::Metrics::new(wire12::MetricsEpoch::new(9).unwrap(), 1, 1, 8, 16)
                .unwrap(),
        };
        let now = Instant::now();
        #[cfg(target_os = "linux")]
        let buffered = fill_send_buffer(&endpoint);
        endpoint
            .queue_reply(
                reply_identity(1),
                reply,
                Some(ring.descriptor().try_clone_to_owned().unwrap()),
                now,
            )
            .unwrap();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(endpoint.flush(now).unwrap(), Flush::Pending);
            let mut filler = vec![0u8; buffered];
            client.read_exact(&mut filler).unwrap();
            assert!(filler.iter().all(|byte| *byte == 42));
        }
        assert_eq!(endpoint.flush(now).unwrap(), Flush::Complete);
        let mut bytes = [0u8; 128];
        let prefix = fd::receive(client.as_fd(), &mut bytes[..4], 1).unwrap();
        assert_eq!(prefix.bytes, 4);
        assert_eq!(prefix.descriptors.len(), 1);
        let remainder = fd::receive(client.as_fd(), &mut bytes[4..], 0).unwrap();
        assert_eq!(remainder.bytes, 124);
        assert_eq!(remainder.descriptors.len(), 0);
        let mut expected = literals::SETUP_EXT.to_vec();
        // Physical extent is an observed kernel value, not a page-size formula.
        expected[40..48].copy_from_slice(&ring.physical_bytes().to_le_bytes());
        #[cfg(target_os = "linux")]
        expected[56..58].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(bytes.as_slice(), expected);
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::fstat(prefix.descriptors[0].as_raw_fd(), &mut status) },
            0
        );
        assert_eq!(status.st_size as u64, ring.physical_bytes());
        assert_eq!(endpoint.flush(now).unwrap(), Flush::Complete);
    }

    #[test]
    fn control_replies_carry_no_rights_and_queueing_preserves_the_pending_reply() {
        let availability = NativeAvailability::new();
        let (host, client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let now = Instant::now();
        let pong = Reply::Pong(wire12::UploadState::Idle);
        endpoint
            .queue_reply(reply_identity(3), pong, None, now)
            .unwrap();
        assert_eq!(
            endpoint
                .queue_reply(reply_identity(3), Reply::Released, None, now)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(matches!(endpoint.poll(now).unwrap(), Receive::Pending));
        assert_eq!(endpoint.flush(now).unwrap(), Flush::Complete);
        let mut bytes = [0u8; 48];
        let received = fd::receive(client.as_fd(), &mut bytes, 0).unwrap();
        assert_eq!(received.bytes, 48);
        assert_eq!(received.descriptors.len(), 0);
        assert_eq!(bytes, literals::PONG_IDLE);
        let (source, source_peer) = pair();
        assert_eq!(
            endpoint
                .queue_reply(reply_identity(3), pong, Some(source.into()), now)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eof(source_peer);
    }

    #[test]
    fn a_backpressured_pong_closes_at_its_absolute_100ms_deadline() {
        let availability = NativeAvailability::new();
        let (host, mut client) = pair();
        let mut endpoint = ControlEndpoint::with_availability(host, &availability).unwrap();
        let flags =
            unsafe { libc::fcntl(endpoint.socket.as_ref().unwrap().as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        let buffered = fill_send_buffer(&endpoint);
        let now = Instant::now();
        endpoint
            .queue_reply(
                reply_identity(3),
                Reply::Pong(wire12::UploadState::Idle),
                None,
                now,
            )
            .unwrap();
        assert_eq!(endpoint.flush(now).unwrap(), Flush::Pending);
        assert_eq!(
            endpoint
                .flush(now + Duration::from_millis(100))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let mut filler = vec![0u8; buffered];
        client.read_exact(&mut filler).unwrap();
        assert!(filler.iter().all(|byte| *byte == 42));
        assert_eof(client);
    }
}
