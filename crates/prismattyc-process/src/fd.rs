// SPDX-License-Identifier: MIT

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

const MAX_FDS: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum AncillaryFailure {
    Truncated,
    Malformed,
    DescriptorCount { received: usize, maximum: usize },
}

impl std::fmt::Display for AncillaryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("truncated ancillary descriptors"),
            Self::Malformed => formatter.write_str("malformed ancillary descriptors"),
            Self::DescriptorCount { received, maximum } => write!(
                formatter,
                "received {received} descriptors, maximum {maximum}"
            ),
        }
    }
}

impl std::error::Error for AncillaryFailure {}

fn cloexec(fd: RawFd) -> io::Result<()> {
    // This descriptor is exclusively owned until the operation returns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic CLOEXEC unavailable",
        ));
    }
    #[cfg(target_os = "macos")]
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Creates connected close-on-exec stream endpoints, without an inheritance gap.
pub fn socket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(target_os = "macos")]
    let _guard = crate::acquire()?;
    let mut pair = [-1; 2];
    #[cfg(target_os = "linux")]
    let flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::SOCK_STREAM;
    // The array has capacity for exactly the two descriptors socketpair writes.
    if unsafe { libc::socketpair(libc::AF_UNIX, flags, 0, pair.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // A successful socketpair call transfers both descriptors to this owner.
    let first = unsafe { OwnedFd::from_raw_fd(pair[0]) };
    let second = unsafe { OwnedFd::from_raw_fd(pair[1]) };
    #[cfg(all(test, target_os = "macos"))]
    crate::test_hooks::window(&[first.as_raw_fd(), second.as_raw_fd()]);
    cloexec(first.as_raw_fd())?;
    cloexec(second.as_raw_fd())?;
    Ok((first, second))
}

/// One nonblocking read and all received descriptors. Protocol framing stays with the caller.
#[derive(Debug)]
pub struct Received {
    pub bytes: usize,
    pub descriptors: Vec<OwnedFd>,
}

/// Receives ancillary descriptors and applies CLOEXEC before any child can inherit them.
/// Truncated or unexpected ancillary records close all delivered descriptors.
pub fn receive(
    socket: BorrowedFd<'_>,
    output: &mut [u8],
    maximum_fds: usize,
) -> io::Result<Received> {
    if output.is_empty() || maximum_fds > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid receive bounds",
        ));
    }
    #[cfg(target_os = "macos")]
    let _guard = crate::acquire()?;
    // Darwin installs the whole rights record before copying ancillary bytes.
    // Reserve the platform's full control capacity, even when policy allows no
    // descriptors. A smaller buffer can hide already-installed descriptors.
    #[cfg(target_os = "macos")]
    let mut ancillary = [0usize; 512];
    // usize storage aligns the kernel's cmsghdr and descriptor payload.
    #[cfg(target_os = "linux")]
    let mut ancillary = [0usize; 32];
    let mut vector = libc::iovec {
        iov_base: output.as_mut_ptr().cast(),
        iov_len: output.len(),
    };
    // All fields are initialized below before recvmsg uses them.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen = mem::size_of_val(&ancillary) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::MSG_DONTWAIT;
    // The iovec and ancillary storage remain valid and exclusively borrowed.
    let count = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, flags) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut descriptors = Vec::new();
    let capacity = mem::size_of_val(&ancillary);
    #[cfg(target_os = "macos")]
    let control_length = message.msg_controllen as usize;
    #[cfg(target_os = "linux")]
    let control_length = message.msg_controllen;
    let mut truncated =
        message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 || control_length > capacity;
    let mut invalid = truncated;
    let initialized = control_length.min(capacity);
    message.msg_controllen = initialized as _;
    let start = message.msg_control as usize;
    let end = start + initialized;
    // Only the kernel-produced initialized control area is traversed.
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        let position = header as usize;
        if position < start
            || position
                .checked_add(mem::size_of::<libc::cmsghdr>())
                .is_none_or(|n| n > end)
        {
            invalid = true;
            break;
        }
        let length = unsafe { (*header).cmsg_len as usize };
        let prefix = unsafe { libc::CMSG_LEN(0) as usize };
        if length < prefix {
            invalid = true;
            break;
        }
        // Darwin preserves the original cmsg_len when MSG_CTRUNC copies only
        // a descriptor prefix. Own and close that prefix before rejecting it.
        let available = end - position;
        let cut = length > available;
        truncated |= cut;
        invalid |= cut;
        let copied = length.min(available);
        if copied < prefix {
            invalid = true;
            break;
        }
        let rights = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        if rights {
            let payload = copied - prefix;
            invalid |= payload % mem::size_of::<RawFd>() != 0;
            let data = unsafe { libc::CMSG_DATA(header) };
            for offset in
                (0..payload / mem::size_of::<RawFd>()).map(|n| n * mem::size_of::<RawFd>())
            {
                // Each SCM_RIGHTS element is a newly owned kernel-delivered FD.
                let raw = unsafe { data.add(offset).cast::<RawFd>().read_unaligned() };
                let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                #[cfg(all(test, target_os = "macos"))]
                crate::test_hooks::window(&[owned.as_raw_fd()]);
                invalid |= cloexec(owned.as_raw_fd()).is_err();
                descriptors.push(owned);
            }
        } else {
            invalid = true;
        }
        if cut {
            break;
        }
        header = unsafe { libc::CMSG_NXTHDR(&message, header) };
    }
    let failure = if truncated {
        Some(AncillaryFailure::Truncated)
    } else if invalid {
        Some(AncillaryFailure::Malformed)
    } else if descriptors.len() > maximum_fds {
        Some(AncillaryFailure::DescriptorCount {
            received: descriptors.len(),
            maximum: maximum_fds,
        })
    } else {
        None
    };
    if let Some(failure) = failure {
        return Err(io::Error::new(io::ErrorKind::InvalidData, failure));
    }
    Ok(Received {
        bytes: count as usize,
        descriptors,
    })
}
