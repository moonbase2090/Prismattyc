// SPDX-License-Identifier: MIT
use super::super::{
    backing::tests as backing,
    frame::{literals, Glyph},
    wire12::{MetricsEpoch, PtyEpoch, SessionId, RING_BYTES, SLOT_BYTES},
};
use super::*;
use std::{
    os::fd::{AsRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) struct Producer {
    address: NonNull<u8>,
    _descriptor: OwnedFd,
}
impl Producer {
    pub(crate) fn minor(&self) -> u16 {
        unsafe {
            u16::from_le_bytes([
                self.address.as_ptr().add(10).read_volatile(),
                self.address.as_ptr().add(11).read_volatile(),
            ])
        }
    }
    pub(crate) fn new(descriptor: OwnedFd) -> Self {
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RING_BYTES as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        Self {
            address: NonNull::new(address.cast()).unwrap(),
            _descriptor: descriptor,
        }
    }
    fn counter(&self, offset: usize) -> &AtomicU64 {
        unsafe { &*self.address.as_ptr().add(offset).cast::<AtomicU64>() }
    }
    pub(crate) fn publish(&self, sequence: u64, literal: &[u8]) {
        let start = 4096 + ((sequence - 1) % 4) as usize * SLOT_BYTES as usize;
        unsafe {
            let address = self.address.as_ptr().add(start);
            std::ptr::write_bytes(address, 0, SLOT_BYTES as usize);
            std::ptr::copy_nonoverlapping(literal.as_ptr(), address, literal.len());
            std::ptr::copy_nonoverlapping(sequence.to_le_bytes().as_ptr(), address.add(8), 8);
        }
        self.counter(64).store(sequence, Ordering::Release);
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        assert_eq!(
            unsafe { libc::munmap(self.address.as_ptr().cast(), RING_BYTES as usize) },
            0
        );
    }
}
fn frames() -> SessionFrames {
    SessionFrames::new(
        RingContext {
            session: SessionId::new(1).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: MetricsEpoch::new(9).unwrap(),
            columns: 1,
            rows: 1,
        },
        ImagePool::isolated(),
    )
    .unwrap()
}
fn descriptor() -> ImageDescriptor {
    let (source, physical) = backing::source();
    ImageDescriptor::validate(source, backing::metadata(physical)).unwrap()
}
fn sequence(n: u64) -> FrameSequence {
    FrameSequence::new(n).unwrap()
}

#[test]
fn consumption_does_not_install_and_fifth_owned_snapshot_waits_for_a_queue_slot() {
    let mut frames = frames();
    let producer = Producer::new(frames.descriptor().try_clone_to_owned().unwrap());
    assert!(!frames.prepare_next().unwrap());
    for n in 1..=4 {
        producer.publish(n, literals::FRAME_INITIAL);
        assert!(frames.prepare_next().unwrap());
    }
    assert!(frames.installed().is_none());
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 4);
    let mut changed = literals::FRAME_INITIAL.to_vec();
    changed[128] = b'X';
    producer.publish(5, &changed);
    assert!(!frames.prepare_next().unwrap());
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 4);
    assert_eq!(frames.install_next(), Ok(Some(sequence(1))));
    assert_eq!(
        frames.installed().unwrap().frame().cells[0].glyph,
        Glyph::Scalar('?')
    );
    assert!(frames.prepare_next().unwrap());
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 5);
    for n in 2..=5 {
        assert_eq!(frames.install_next(), Ok(Some(sequence(n))));
    }
    assert_eq!(
        frames.installed().unwrap().frame().cells[0].glyph,
        Glyph::Scalar('X')
    );
    assert_eq!(frames.install_next(), Ok(None));
}

#[test]
fn an_unresolved_image_never_acknowledges_the_frame_or_replaces_the_installed_scene() {
    let mut frames = frames();
    let producer = Producer::new(frames.descriptor().try_clone_to_owned().unwrap());
    producer.publish(1, literals::FRAME_INITIAL);
    frames.prepare_next().unwrap();
    frames.install_next().unwrap();
    producer.publish(2, literals::FRAME_PLACEMENT);
    assert_eq!(
        frames.prepare_next(),
        Err(SceneError::Resource(RegistryError::UnknownHandle))
    );
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 1);
    assert_eq!(
        frames.installed().unwrap().frame().context.sequence.get(),
        1
    );
    assert_eq!(
        frames.installed().unwrap().frame().cells[0].glyph,
        Glyph::Scalar('?')
    );
}

#[test]
fn retiring_waits_for_the_installed_omission_and_a_real_retained_scene() {
    let mut frames = frames();
    let now = Instant::now();
    let producer = Producer::new(frames.descriptor().try_clone_to_owned().unwrap());
    producer.publish(1, literals::FRAME_INITIAL);
    frames.prepare_next().unwrap();
    frames.install_next().unwrap();
    assert!(matches!(
        frames.put(descriptor(), now).unwrap(),
        Reply::Admitted { .. }
    ));
    frames.resources.finish_for_test().unwrap();
    producer.publish(2, literals::FRAME_PLACEMENT);
    frames.prepare_next().unwrap();
    frames.install_next().unwrap();
    let retained = Arc::clone(frames.installed().unwrap());
    assert_eq!(retained.images()[0].pixels.pixels(), [255, 0, 0, 128]);
    assert_eq!(retained.images()[0].placement.size, [8, 16]);
    producer.publish(3, literals::FRAME_OMISSION);
    frames.prepare_next().unwrap();
    let handle = ResourceHandle::new(1).unwrap();
    assert_eq!(frames.retire(handle, sequence(3)), Ok(Retirement::Busy));
    frames.install_next().unwrap();
    assert!(frames.installed().unwrap().images().is_empty());
    assert_eq!(frames.retire(handle, sequence(3)), Ok(Retirement::Busy));
    drop(retained);
    assert_eq!(frames.retire(handle, sequence(3)), Ok(Retirement::Retired));
    assert_eq!(frames.pong(now), Ok(UploadState::Idle));
    producer.publish(4, literals::FRAME_PLACEMENT);
    assert_eq!(
        frames.prepare_next(),
        Err(SceneError::Resource(RegistryError::UnknownHandle))
    );
    assert_eq!(producer.counter(128).load(Ordering::Acquire), 3);
}

#[test]
fn an_exec_child_publishes_into_the_real_inherited_ring_before_installation() {
    const CHILD_FD: i32 = 198;
    const CHILD_ENV: &str = "PRISMATTYC_NATIVE_PROOF_RING_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        use std::os::fd::FromRawFd;
        let producer = Producer::new(unsafe { OwnedFd::from_raw_fd(CHILD_FD) });
        assert_eq!(producer.counter(64).load(Ordering::Acquire), 0);
        producer.publish(1, literals::FRAME_INITIAL);
        return;
    }
    use std::os::unix::process::CommandExt;
    let mut frames = frames();
    let descriptor = frames.descriptor().try_clone_to_owned().unwrap();
    let raw = descriptor.as_raw_fd();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    let module = module_path!().split_once("::").unwrap().1;
    let test = format!(
        "{module}::an_exec_child_publishes_into_the_real_inherited_ring_before_installation"
    );
    command
        .args([test.as_str(), "--exact"])
        .env(CHILD_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if raw != CHILD_FD && libc::dup2(raw, CHILD_FD) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(CHILD_FD, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = prismattyc_process::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(frames.installed().is_none());
    assert!(frames.prepare_next().unwrap());
    assert!(frames.installed().is_none());
    assert_eq!(frames.install_next(), Ok(Some(sequence(1))));
    assert_eq!(
        frames.installed().unwrap().grid().cell(0, 0).unwrap().text,
        super::super::grid::Text::Scalar('?')
    );
}
