// SPDX-License-Identifier: MIT
use super::super::backing::tests as backing;
use super::*;
use std::num::NonZeroU32;

fn registry() -> ImageRegistry {
    ImageRegistry::new(
        SessionId::new(1).unwrap(),
        PtyEpoch::new(7).unwrap(),
        ImagePool::isolated(),
    )
}

fn descriptor(id: u32, generation: u64, wrong_digest: bool) -> ImageDescriptor {
    let (source, physical) = backing::source();
    let mut metadata = backing::metadata(physical);
    metadata.image_id = NonZeroU32::new(id).unwrap();
    metadata.source_generation = generation;
    if wrong_digest {
        metadata.digest[0] ^= 1;
    }
    ImageDescriptor::validate(source, metadata).unwrap()
}

fn admitted(reply: Reply) -> ResourceHandle {
    match reply {
        Reply::Admitted { handle, .. } => handle,
        _ => panic!("upload was not admitted"),
    }
}

fn finish(registry: &mut ImageRegistry) -> Result<(), RegistryError> {
    let mut job = registry.running.take().unwrap();
    let completed = job.worker.wait_for_test();
    registry.complete(job, completed)
}

fn frame(n: u64) -> FrameSequence {
    FrameSequence::new(n).unwrap()
}
fn progress(n: u64) -> SceneProgress {
    let mut p = SceneProgress::default();
    for n in 1..=n {
        p.consume(frame(n)).unwrap();
        p.install(frame(n)).unwrap();
    }
    p
}
fn placement(handle: ResourceHandle) -> Placement {
    Placement {
        id: 1,
        depth: 0,
        handle,
        crop: [0, 0, 1, 1],
        destination: [0, 0],
        size: [1, 1],
        opacity: 255,
        ordinal: 0,
    }
}

#[test]
fn generation_zero_is_live_and_duplicate_uploads_keep_the_same_handle_and_pixels() {
    let mut r = registry();
    let now = Instant::now();
    assert_eq!(
        r.put(descriptor(1, 0, false), now),
        Ok(Reply::Admitted {
            handle: ResourceHandle::new(1).unwrap(),
            status: Admission::Pending
        })
    );
    let duplicate = admitted(r.put(descriptor(1, 0, false), now).unwrap());
    assert_eq!(duplicate.get(), 1);
    if r.running.is_some() {
        finish(&mut r).unwrap();
    }
    assert_eq!(
        r.put(descriptor(1, 0, false), now),
        Ok(Reply::Admitted {
            handle: ResourceHandle::new(1).unwrap(),
            status: Admission::ExistingReady
        })
    );
    let pixels = r.resolve(frame(1), &[placement(duplicate)]).unwrap();
    assert_eq!(pixels[0].pixels.pixels(), [255, 0, 0, 128]);
    assert_eq!(
        (pixels[0].pixels.width(), pixels[0].pixels.height()),
        (1, 1)
    );
    assert_eq!(
        r.pong(now),
        Ok(UploadState::Ready(ResourceHandle::new(1).unwrap()))
    );
}

#[test]
fn changed_digest_cannot_rebind_a_live_key_but_a_new_generation_gets_a_new_handle() {
    let mut r = registry();
    let now = Instant::now();
    admitted(r.put(descriptor(1, 0, false), now).unwrap());
    finish(&mut r).unwrap();
    assert_eq!(
        r.put(descriptor(1, 0, true), now),
        Err(RegistryError::ChangedLiveKey)
    );
    assert_eq!(
        admitted(r.put(descriptor(1, 1, false), now).unwrap()).get(),
        2
    );
    finish(&mut r).unwrap();
    assert_eq!(
        r.pong(now),
        Ok(UploadState::Ready(ResourceHandle::new(2).unwrap()))
    );
}

#[test]
fn deadline_report_survives_cleanup_and_the_next_admission_releases_the_failed_key() {
    let mut r = registry();
    let now = Instant::now();
    admitted(r.put(descriptor(1, 0, false), now).unwrap());
    let late = now + Duration::from_secs(2);
    let failure = UploadState::Failed {
        handle: ResourceHandle::new(1).unwrap(),
        error: UploadFailure::Deadline,
    };
    assert_eq!(r.pong(late), Ok(failure));
    if r.running.is_some() {
        finish(&mut r).unwrap();
    }
    assert_eq!(r.pong(late), Ok(failure));
    assert_eq!(
        admitted(r.put(descriptor(1, 0, true), late).unwrap()).get(),
        2
    );
    assert_eq!(finish(&mut r), Err(RegistryError::DigestMismatch));
}

#[test]
fn resolving_requires_ready_ownership_and_an_in_bounds_crop() {
    let mut r = registry();
    let now = Instant::now();
    let handle = admitted(r.put(descriptor(1, 0, false), now).unwrap());
    assert!(matches!(
        r.resolve(frame(1), &[placement(handle)]),
        Err(RegistryError::UnreadyHandle)
    ));
    finish(&mut r).unwrap();
    let mut invalid = placement(handle);
    invalid.crop = [1, 0, 1, 1];
    assert!(matches!(
        r.resolve(frame(1), &[invalid]),
        Err(RegistryError::InvalidCrop)
    ));
    assert!(matches!(
        r.resolve(frame(1), &[placement(ResourceHandle::new(2).unwrap())]),
        Err(RegistryError::UnknownHandle)
    ));
    let other = registry();
    assert!(matches!(
        {
            let mut other = other;
            other.resolve(frame(1), &[placement(handle)])
        },
        Err(RegistryError::UnknownHandle)
    ));
    assert_eq!(
        r.resolve(frame(1), &[placement(handle)]).unwrap()[0]
            .pixels
            .pixels(),
        [255, 0, 0, 128]
    );
}

#[test]
fn retirement_waits_for_omission_and_owned_scene_references_then_reappearance_allocates() {
    let mut r = registry();
    let now = Instant::now();
    let handle = admitted(r.put(descriptor(1, 0, false), now).unwrap());
    finish(&mut r).unwrap();
    let scene = r.resolve(frame(1), &[placement(handle)]).unwrap();
    assert_eq!(
        r.retire(handle, frame(2), progress(1)),
        Ok(Retirement::Busy)
    );
    assert_eq!(
        r.retire(handle, frame(2), progress(2)),
        Ok(Retirement::Busy)
    );
    assert!(matches!(
        r.resolve(frame(2), &[placement(handle)]),
        Err(RegistryError::Retirement(
            RetirementError::ReferenceAfterOmission
        ))
    ));
    drop(scene);
    assert_eq!(
        r.retire(handle, frame(2), progress(2)),
        Ok(Retirement::Retired)
    );
    assert_eq!(r.pong(now), Ok(UploadState::Idle));
    assert!(matches!(
        r.resolve(frame(3), &[placement(handle)]),
        Err(RegistryError::UnknownHandle)
    ));
    assert_eq!(
        r.retire(handle, frame(2), progress(2)),
        Err(RegistryError::UnknownHandle)
    );
    assert_eq!(
        admitted(r.put(descriptor(1, 0, true), now).unwrap()).get(),
        2
    );
    assert_eq!(finish(&mut r), Err(RegistryError::DigestMismatch));
}

#[test]
fn retiring_an_older_handle_keeps_the_latest_upload_report() {
    let mut r = registry();
    let now = Instant::now();
    let first = admitted(r.put(descriptor(1, 0, false), now).unwrap());
    finish(&mut r).unwrap();
    let second = admitted(r.put(descriptor(1, 1, false), now).unwrap());
    finish(&mut r).unwrap();
    assert_eq!(
        r.retire(first, frame(1), progress(1)),
        Ok(Retirement::Retired)
    );
    assert_eq!(r.pong(now), Ok(UploadState::Ready(second)));
}

#[test]
fn handle_exhaustion_ends_admission_before_wrap_but_live_duplicates_remain_identical() {
    let mut r = registry();
    let now = Instant::now();
    r.next_handle = ResourceHandle::new(u64::MAX);
    assert_eq!(
        admitted(r.put(descriptor(1, 0, false), now).unwrap()).get(),
        u64::MAX
    );
    finish(&mut r).unwrap();
    assert_eq!(
        admitted(r.put(descriptor(1, 0, false), now).unwrap()).get(),
        u64::MAX
    );
    assert_eq!(
        r.put(descriptor(1, 1, false), now),
        Err(RegistryError::HandleExhausted)
    );
}

#[test]
fn the_resource_cap_declines_without_evicting_live_pixels() {
    let mut r = registry();
    let now = Instant::now();
    for generation in 0..256 {
        assert_eq!(
            admitted(r.put(descriptor(1, generation, false), now).unwrap()).get(),
            generation + 1
        );
        finish(&mut r).unwrap();
    }
    assert_eq!(
        r.put(descriptor(1, 256, false), now),
        Ok(Reply::Declined(Decline::HandleBudget))
    );
    let first = ResourceHandle::new(1).unwrap();
    assert_eq!(
        r.resolve(frame(1), &[placement(first)]).unwrap()[0]
            .pixels
            .pixels(),
        [255, 0, 0, 128]
    );
    assert_eq!(
        admitted(r.put(descriptor(1, 0, false), now).unwrap()),
        first
    );
}
