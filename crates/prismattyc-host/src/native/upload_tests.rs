// SPDX-License-Identifier: MIT
use super::super::{
    backing::{tests as backing, ImageDescriptor},
    pool::ImagePool,
    wire12::Decline,
};
use super::*;
#[cfg(target_os = "macos")]
use std::os::fd::AsFd;
use std::sync::{mpsc, Mutex};

fn identity() -> UploadIdentity {
    UploadIdentity {
        session: SessionId::new(1).unwrap(),
        epoch: PtyEpoch::new(7).unwrap(),
        handle: ResourceHandle::new(9).unwrap(),
    }
}
fn descriptor(wrong_digest: bool) -> ImageDescriptor {
    let (source, physical) = backing::source();
    let mut metadata = backing::metadata(physical);
    if wrong_digest {
        metadata.digest[0] ^= 1;
    }
    ImageDescriptor::validate(source, metadata).unwrap()
}
fn ready(outcome: UploadOutcome) -> Arc<OwnedImage> {
    match outcome {
        UploadOutcome::Ready(pixels) => pixels,
        _ => panic!("upload did not become READY"),
    }
}
fn paused_control(time: Arc<Mutex<Instant>>) -> (Control, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (copied, observed) = mpsc::channel();
    let (resume, proceed) = mpsc::channel();
    (
        Control {
            clock: Clock::Manual(time),
            hooks: Some(CopyHooks { copied, proceed }),
            exited: None,
        },
        observed,
        resume,
    )
}

#[test]
fn real_upload_produces_the_reviewed_pixel_bytes_and_preserves_job_identity() {
    let pool = ImagePool::isolated();
    let admitted = pool.admit(descriptor(false)).unwrap();
    let result = UploadWorker::start(admitted, identity(), Instant::now())
        .unwrap()
        .finish();
    assert_eq!(result.identity, identity());
    let pixels = ready(result.outcome);
    assert_eq!((pixels.width(), pixels.height()), (1, 1));
    assert_eq!(pixels.pixels(), [255, 0, 0, 128]);
    let _next = pool.admit(descriptor(false)).unwrap();
    assert!(pool.admit(descriptor(false)).is_ok());
}

#[test]
fn digest_mismatch_is_a_protocol_failure_and_releases_the_worker_permit() {
    let pool = ImagePool::isolated();
    let admitted = pool.admit(descriptor(true)).unwrap();
    let result = UploadWorker::start(admitted, identity(), Instant::now())
        .unwrap()
        .finish();
    assert!(matches!(result.outcome, UploadOutcome::DigestMismatch));
    let _first = pool.admit(descriptor(false)).unwrap();
    assert!(pool.admit(descriptor(false)).is_ok());
}

#[test]
fn a_deadline_reached_after_copy_prevents_hashing_and_ready_publication() {
    let pool = ImagePool::isolated();
    let now = Instant::now();
    let time = Arc::new(Mutex::new(now));
    let (control, copied, resume) = paused_control(Arc::clone(&time));
    let mut worker = UploadWorker::start_inner(
        pool.admit(descriptor(false)).unwrap(),
        identity(),
        now,
        control,
    )
    .unwrap();
    copied.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(worker.take_finished().is_none());
    *time.lock().unwrap() = now + Duration::from_secs(2);
    resume.send(()).unwrap();
    assert!(matches!(
        worker.finish().outcome,
        UploadOutcome::Failed(UploadFailure::Deadline)
    ));
}

#[test]
fn cancellation_after_a_real_copy_never_publishes_ready_pixels() {
    let pool = ImagePool::isolated();
    let now = Instant::now();
    let (control, copied, resume) = paused_control(Arc::new(Mutex::new(now)));
    let worker = UploadWorker::start_inner(
        pool.admit(descriptor(false)).unwrap(),
        identity(),
        now,
        control,
    )
    .unwrap();
    copied.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.cancel();
    resume.send(()).unwrap();
    assert!(matches!(worker.finish().outcome, UploadOutcome::Cancelled));
    let _first = pool.admit(descriptor(false)).unwrap();
    assert!(pool.admit(descriptor(false)).is_ok());
}

#[test]
fn dropping_a_worker_cancels_without_joining_and_keeps_credits_until_cleanup() {
    let pool = ImagePool::isolated();
    let now = Instant::now();
    let (mut control, copied, resume) = paused_control(Arc::new(Mutex::new(now)));
    let (cleaned, cleanup) = mpsc::channel();
    control.exited = Some(cleaned);
    let worker = UploadWorker::start_inner(
        pool.admit(descriptor(false)).unwrap(),
        identity(),
        now,
        control,
    )
    .unwrap();
    copied.recv_timeout(Duration::from_secs(2)).unwrap();
    let (dropped, saw_drop) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(worker);
        dropped.send(()).unwrap();
    });
    saw_drop.recv_timeout(Duration::from_secs(2)).unwrap();
    dropper.join().unwrap();
    let _second = pool.admit(descriptor(false)).unwrap();
    assert!(matches!(pool.admit(descriptor(false)), Err(Decline::Busy)));
    resume.send(()).unwrap();
    assert!(cleanup.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(pool.admit(descriptor(false)).is_ok());
}

#[test]
#[cfg(target_os = "macos")]
fn hashing_uses_the_owned_copy_when_the_mutable_source_changes_after_copying() {
    let pool = ImagePool::isolated();
    let (source, physical) = backing::source();
    let descriptor =
        ImageDescriptor::validate(source.try_clone().unwrap(), backing::metadata(physical))
            .unwrap();
    let now = Instant::now();
    let (control, copied, resume) = paused_control(Arc::new(Mutex::new(now)));
    let worker =
        UploadWorker::start_inner(pool.admit(descriptor).unwrap(), identity(), now, control)
            .unwrap();
    copied.recv_timeout(Duration::from_secs(2)).unwrap();
    backing::overwrite(source.as_fd(), physical, [1, 2, 3, 4]);
    resume.send(()).unwrap();
    let pixels = ready(worker.finish().outcome);
    assert_eq!(pixels.pixels(), [255, 0, 0, 128]);
}
