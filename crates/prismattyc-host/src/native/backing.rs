// SPDX-License-Identifier: MIT
//! Fixed-extent shared objects. No borrowed Rust slices cover mutable peer memory.

use super::wire12::{BackingKind, ImageUpload, RING_BYTES};
use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::NonNull,
};

pub struct ImageDescriptor {
    descriptor: OwnedFd,
    metadata: ImageUpload,
}

impl ImageDescriptor {
    /// Takes exclusive ownership of a received descriptor before inspecting it.
    pub fn validate(descriptor: OwnedFd, metadata: ImageUpload) -> io::Result<Self> {
        validate_extent(
            descriptor.as_fd(),
            metadata.backing,
            metadata.logical_bytes,
            Some(metadata.physical_bytes),
            true,
        )?;
        Ok(Self {
            descriptor,
            metadata,
        })
    }

    pub fn logical_bytes(&self) -> u64 {
        self.metadata.logical_bytes
    }
    pub fn physical_bytes(&self) -> u64 {
        self.metadata.physical_bytes
    }

    pub fn metadata(&self) -> ImageUpload {
        self.metadata
    }

    /// Called by the admitted worker after both credits and its permit exist.
    pub fn map(self) -> io::Result<ImageBacking> {
        Ok(ImageBacking {
            mapping: Mapping::map(
                self.descriptor,
                self.metadata.logical_bytes,
                self.metadata.physical_bytes,
                false,
            )?,
        })
    }
}

pub struct ImageBacking {
    mapping: Mapping,
}

impl ImageBacking {
    pub fn logical_bytes(&self) -> usize {
        self.mapping.logical
    }

    pub fn physical_bytes(&self) -> u64 {
        self.mapping.physical
    }

    /// Reads only the logical extent into owned memory; hashing uses that copy.
    pub fn copy_chunk(&self, offset: usize, destination: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(destination.len())
            .filter(|end| *end <= self.mapping.logical)
            .ok_or_else(|| invalid("image copy outside logical extent"))?;
        for (index, output) in (offset..end).zip(destination) {
            // The fixed backing cannot shrink. Volatile raw reads create no
            // shared references while another process may change the bytes.
            *output = unsafe { self.mapping.address.as_ptr().add(index).read_volatile() };
        }
        Ok(())
    }
}

pub struct RingBacking {
    mapping: Mapping,
}

impl RingBacking {
    pub fn create() -> io::Result<Self> {
        let descriptor = create_fixed(RING_BYTES)?;
        let physical =
            validate_extent(descriptor.as_fd(), platform_kind(), RING_BYTES, None, false)?;
        Ok(Self {
            mapping: Mapping::map(descriptor, RING_BYTES, physical, true)?,
        })
    }

    pub fn descriptor(&self) -> BorrowedFd<'_> {
        self.mapping.descriptor.as_fd()
    }

    pub fn physical_bytes(&self) -> u64 {
        self.mapping.physical
    }

    pub(super) fn address(&mut self) -> NonNull<u8> {
        self.mapping.address
    }
}

struct Mapping {
    descriptor: OwnedFd,
    address: NonNull<u8>,
    logical: usize,
    physical: u64,
}

impl Mapping {
    fn map(descriptor: OwnedFd, logical: u64, physical: u64, writable: bool) -> io::Result<Self> {
        let logical = usize::try_from(logical).map_err(|_| invalid("mapping length"))?;
        let protection = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // The descriptor was checked for fixed extent before mapping exactly
        // logical bytes. Never map or expose the rounded Darwin suffix.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                logical,
                protection,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let address = match NonNull::new(address.cast()) {
            Some(address) => address,
            None => {
                unsafe { libc::munmap(address, logical) };
                return Err(invalid("null mapping address"));
            }
        };
        Ok(Self {
            descriptor,
            address,
            logical,
            physical,
        })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // This mapping owns its exact logical range and no Rust slice aliases it.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.logical) };
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn platform_kind() -> BackingKind {
    #[cfg(target_os = "macos")]
    {
        BackingKind::DarwinFixedPosix
    }
    #[cfg(target_os = "linux")]
    {
        BackingKind::LinuxSealed
    }
}

fn validate_extent(
    descriptor: BorrowedFd<'_>,
    kind: BackingKind,
    logical: u64,
    declared: Option<u64>,
    image: bool,
) -> io::Result<u64> {
    if logical == 0 || kind != platform_kind() {
        return Err(invalid("unsupported native backing"));
    }
    let raw = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(invalid("native descriptor lacks atomic close-on-exec"));
    }
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(raw, &mut status) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let physical = u64::try_from(status.st_size).map_err(|_| invalid("negative backing extent"))?;
    if status.st_uid != unsafe { libc::geteuid() }
        || declared.is_some_and(|value| value != physical)
    {
        return Err(invalid("native backing identity or extent"));
    }
    #[cfg(target_os = "macos")]
    {
        if !cfg!(target_arch = "aarch64") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "translated or unproven native execution",
            ));
        }
        let _ = image;
        let mut typed: PshmFdInfo = unsafe { std::mem::zeroed() };
        // proc_info.h's typed PSHM record, including the full name array.
        let result = unsafe {
            libc::proc_pidfdinfo(
                libc::getpid(),
                raw,
                5,
                (&mut typed as *mut PshmFdInfo).cast(),
                std::mem::size_of_val(&typed) as _,
            )
        };
        if result != std::mem::size_of_val(&typed) as i32
            || typed.pshm.stat.vst_uid != unsafe { libc::geteuid() }
            || typed.pshm.stat.vst_size != status.st_size
            || physical < logical
            || logical
                .checked_add(65_536)
                .is_none_or(|limit| physical >= limit)
        {
            return Err(invalid("typed fixed PSHM backing required"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if status.st_mode & libc::S_IFMT != libc::S_IFREG || physical != logical {
            return Err(invalid("sealed regular backing extent"));
        }
        let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
        let required =
            libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | if image { libc::F_SEAL_WRITE } else { 0 };
        if seals < 0 || seals & required != required {
            return Err(invalid("native backing lacks required seals"));
        }
    }
    Ok(physical)
}

fn create_unallocated() -> io::Result<OwnedFd> {
    #[cfg(target_os = "macos")]
    let raw = {
        if !cfg!(target_arch = "aarch64") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unproven native execution",
            ));
        }
        let mut random = [0u8; 12];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        // Darwin limits PSHM names to 31 bytes; this unlinked name uses 28.
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = std::ffi::CString::new(format!("/lni{suffix}")).unwrap();
        // XNU sets FP_CLOEXEC before publishing the newly reserved descriptor.
        let raw = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(raw) };
            return Err(error);
        }
        raw
    };
    #[cfg(target_os = "linux")]
    let raw = unsafe {
        libc::memfd_create(
            c"native-ring".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // A successful creation transfers exclusive descriptor ownership.
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(descriptor)
}

fn create_fixed(logical: u64) -> io::Result<OwnedFd> {
    let length = libc::off_t::try_from(logical).map_err(|_| invalid("backing length"))?;
    if length <= 0 {
        return Err(invalid("empty backing length"));
    }
    let descriptor = create_unallocated()?;
    let raw = descriptor.as_raw_fd();
    if unsafe { libc::ftruncate(raw, length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    if unsafe {
        libc::fcntl(
            raw,
            libc::F_ADD_SEALS,
            libc::F_SEAL_GROW | libc::F_SEAL_SHRINK,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(descriptor)
}

// Exact sys/proc_info.h layout. libc supplies vinfo_stat but not pshm_fdinfo.
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcFileInfo {
    open_flags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guard_flags: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct PshmInfo {
    stat: libc::vinfo_stat,
    mapping_address: u64,
    name: [libc::c_char; 1024],
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct PshmFdInfo {
    file: ProcFileInfo,
    pshm: PshmInfo,
}

#[cfg(target_os = "macos")]
const _: () = {
    assert!(std::mem::size_of::<PshmFdInfo>() == 1192);
    assert!(std::mem::offset_of!(PshmFdInfo, pshm) == 24);
    assert!(std::mem::offset_of!(libc::vinfo_stat, vst_uid) == 16);
    assert!(std::mem::offset_of!(libc::vinfo_stat, vst_size) == 88);
};

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::Read,
        num::NonZeroU32,
        os::unix::{fs::OpenOptionsExt, net::UnixStream},
    };

    fn transfer(descriptor: BorrowedFd<'_>) -> OwnedFd {
        let (sender, receiver) = prismattyc_process::fd::socket_pair().unwrap();
        let mut marker = [0x7fu8];
        let mut vector = libc::iovec {
            iov_base: marker.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(4) as _;
            libc::CMSG_DATA(header)
                .cast::<libc::c_int>()
                .write_unaligned(descriptor.as_raw_fd());
            assert_eq!(
                libc::sendmsg(sender.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
                1
            );
        }
        let mut byte = [0];
        let received = prismattyc_process::fd::receive(receiver.as_fd(), &mut byte, 1).unwrap();
        assert_eq!(byte, [0x7f]);
        assert_eq!(received.descriptors.len(), 1);
        received.descriptors.into_iter().next().unwrap()
    }

    pub(crate) fn metadata(physical: u64) -> ImageUpload {
        ImageUpload {
            image_id: NonZeroU32::new(1).unwrap(),
            source_generation: 0,
            width: NonZeroU32::new(1).unwrap(),
            height: NonZeroU32::new(1).unwrap(),
            logical_bytes: 4,
            physical_bytes: physical,
            backing: platform_kind(),
            digest: [
                0xc4, 0x0b, 0x4a, 0xc0, 0x51, 0x4f, 0x62, 0xad, 0x2d, 0x86, 0x77, 0xe8, 0xf8, 0xbc,
                0x53, 0xed, 0x5a, 0x80, 0x58, 0x2c, 0x5e, 0xa3, 0xaf, 0xc3, 0x95, 0xe2, 0xd0, 0xdf,
                0x6d, 0x88, 0x28, 0xa1,
            ],
        }
    }

    pub(crate) fn source() -> (OwnedFd, u64) {
        let descriptor = create_fixed(4).unwrap();
        let physical =
            validate_extent(descriptor.as_fd(), platform_kind(), 4, None, false).unwrap();
        let mapping = Mapping::map(descriptor.try_clone().unwrap(), 4, physical, true).unwrap();
        for (offset, value) in [255, 0, 0, 128].into_iter().enumerate() {
            unsafe { mapping.address.as_ptr().add(offset).write_volatile(value) };
        }
        drop(mapping);
        #[cfg(target_os = "linux")]
        assert_eq!(
            unsafe {
                libc::fcntl(
                    descriptor.as_raw_fd(),
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_WRITE,
                )
            },
            0
        );
        (descriptor, physical)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn overwrite(source: BorrowedFd<'_>, physical: u64, bytes: [u8; 4]) {
        let mapping =
            Mapping::map(source.try_clone_to_owned().unwrap(), 4, physical, true).unwrap();
        for (offset, value) in bytes.into_iter().enumerate() {
            unsafe { mapping.address.as_ptr().add(offset).write_volatile(value) };
        }
    }

    #[test]
    fn ring_backing_is_fixed_and_close_on_exec_at_creation() {
        let ring = RingBacking::create().unwrap();
        let flags = unsafe { libc::fcntl(ring.descriptor().as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let physical = ring.physical_bytes();
        assert!((4_198_400..4_263_936).contains(&physical));
        for length in [0, physical + 65_536] {
            assert_eq!(
                unsafe { libc::ftruncate(ring.descriptor().as_raw_fd(), length as _) },
                -1
            );
        }
        assert_eq!(
            validate_extent(
                ring.descriptor(),
                platform_kind(),
                RING_BYTES,
                Some(physical),
                false
            )
            .unwrap(),
            physical
        );
        #[cfg(target_os = "linux")]
        assert_eq!(physical, 4_198_400);
    }

    #[test]
    fn transferred_image_copies_only_logical_bytes_into_owned_storage() {
        let (source, physical) = source();
        let backing = ImageDescriptor::validate(transfer(source.as_fd()), metadata(physical))
            .unwrap()
            .map()
            .unwrap();
        assert_eq!(backing.logical_bytes(), 4);
        assert_eq!(backing.physical_bytes(), physical);
        let mut pixels = [0; 4];
        backing.copy_chunk(0, &mut pixels).unwrap();
        assert_eq!(pixels, [255, 0, 0, 128]);
        let mut middle = [0; 2];
        backing.copy_chunk(2, &mut middle).unwrap();
        assert_eq!(middle, [0, 128]);
        for offset in [1, 4, usize::MAX] {
            let mut untouched = [17; 4];
            assert!(backing.copy_chunk(offset, &mut untouched).is_err());
            assert_eq!(untouched, [17; 4]);
        }
        #[cfg(target_os = "macos")]
        {
            let writer = Mapping::map(source, 4, physical, true).unwrap();
            unsafe { writer.address.as_ptr().write_volatile(1) };
            assert_eq!(pixels, [255, 0, 0, 128]);
            backing.copy_chunk(0, &mut pixels).unwrap();
            assert_eq!(pixels, [1, 0, 0, 128]);
        }
    }

    #[test]
    fn sockets_regular_files_and_unallocated_objects_are_rejected_after_transfer() {
        let (socket, peer) = prismattyc_process::fd::socket_pair().unwrap();
        let received = transfer(socket.as_fd());
        drop(socket);
        assert!(ImageDescriptor::validate(received, metadata(4)).is_err());
        let mut peer: UnixStream = peer.into();
        peer.set_nonblocking(true).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);

        let mut nonce = [0; 8];
        getrandom::fill(&mut nonce).unwrap();
        let path =
            std::env::temp_dir().join(format!("native-backing-{:016x}", u64::from_le_bytes(nonce)));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file.set_len(4).unwrap();
        assert!(ImageDescriptor::validate(transfer(file.as_fd()), metadata(4)).is_err());
        let empty = create_unallocated().unwrap();
        assert!(ImageDescriptor::validate(transfer(empty.as_fd()), metadata(4)).is_err());
    }

    #[test]
    fn forged_extent_or_backing_kind_never_reaches_mapping() {
        let (source, physical) = source();
        for declared in [0, physical + 1, physical + 65_536, u64::MAX] {
            assert!(
                ImageDescriptor::validate(transfer(source.as_fd()), metadata(declared)).is_err()
            );
        }
        let mut other = metadata(physical);
        other.backing = match platform_kind() {
            BackingKind::DarwinFixedPosix => BackingKind::LinuxSealed,
            BackingKind::LinuxSealed => BackingKind::DarwinFixedPosix,
        };
        assert!(ImageDescriptor::validate(transfer(source.as_fd()), other).is_err());
    }

    #[test]
    fn missing_close_on_exec_fails_without_a_flag_setting_fallback() {
        let (source, physical) = source();
        let received = transfer(source.as_fd());
        assert_eq!(
            unsafe { libc::fcntl(received.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        assert!(ImageDescriptor::validate(received, metadata(physical)).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn image_requires_a_write_seal_in_addition_to_fixed_length() {
        let descriptor = create_fixed(4).unwrap();
        assert!(ImageDescriptor::validate(transfer(descriptor.as_fd()), metadata(4)).is_err());
        assert_eq!(
            unsafe {
                libc::fcntl(
                    descriptor.as_raw_fd(),
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_WRITE,
                )
            },
            0
        );
        assert!(ImageDescriptor::validate(transfer(descriptor.as_fd()), metadata(4)).is_ok());
    }
}
