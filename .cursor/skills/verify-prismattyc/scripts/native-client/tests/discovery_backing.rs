// SPDX-License-Identifier: MIT
use lunatui_prismattyc::{discover_from_input, NativeDiscovery, NativePolicy};
use native_client_probe::{
    backing::{PreparedImage, KIND},
    session::Offer,
};
use native_component_proof::{
    backing::ImageDescriptor,
    wire12::{BackingKind, ImageUpload},
};
use std::{num::NonZeroU32, sync::atomic::AtomicBool};

#[test]
fn a_complete_capability_reply_is_read_without_waiting_for_already_buffered_bytes() {
    use std::{
        io::Write,
        os::{fd::AsFd, unix::net::UnixStream},
        time::{Duration, Instant},
    };
    let (sender, receiver) = prismattyc_process::fd::socket_pair().unwrap();
    let mut sender = UnixStream::from(sender);
    sender.write_all(b"\x1b_Prismattyc;cap;r;id=77;v=0.4;features=native.images.v1;native.offer=0123456789abcdef0123456789abcdef;native.generation=19\x1b\\").unwrap();
    let offer = native_client_probe::session::read_offer(
        receiver.as_fd(),
        77,
        Instant::now() + Duration::from_millis(200),
    )
    .unwrap();
    assert_eq!(offer.epoch, 19);
    assert_eq!(offer.id, 77);
}

#[test]
fn images_only_selects_minor_two_and_the_existing_grid_client_keeps_ansi() {
    let reply = b"\x1b_Prismattyc;cap;r;id=77;v=0.4;features=native.images.v1;native.offer=0123456789abcdef0123456789abcdef;native.generation=19\x1b\\";
    let offer = Offer::parse(reply, 77).unwrap();
    assert_eq!(offer.epoch, 19);
    assert_eq!(
        offer.value,
        [1, 35, 69, 103, 137, 171, 205, 239, 1, 35, 69, 103, 137, 171, 205, 239]
    );
    let legacy = discover_from_input(NativePolicy::DirectLocal, 77, reply);
    assert_eq!(legacy.discovery, NativeDiscovery::Unavailable);
    assert!(legacy.uses_ansi());
    assert!(!legacy.native_active());
    assert!(Offer::parse(reply, 78).is_err());
    assert!(Offer::parse(
        std::str::from_utf8(reply)
            .unwrap()
            .replace("native.images.v1", "native.grid.v1")
            .as_bytes(),
        77
    )
    .is_err());
}

#[test]
fn a_real_prepared_platform_object_is_accepted_and_read_by_the_host() {
    let pixels = [255, 0, 0, 128];
    let prepared = PreparedImage::prepare(&pixels).unwrap();
    assert_eq!(
        prepared.digest,
        [
            0xc4, 0x0b, 0x4a, 0xc0, 0x51, 0x4f, 0x62, 0xad, 0x2d, 0x86, 0x77, 0xe8, 0xf8, 0xbc,
            0x53, 0xed, 0x5a, 0x80, 0x58, 0x2c, 0x5e, 0xa3, 0xaf, 0xc3, 0x95, 0xe2, 0xd0, 0xdf,
            0x6d, 0x88, 0x28, 0xa1
        ]
    );
    let source = ImageDescriptor::validate(
        prepared.descriptor,
        ImageUpload {
            image_id: NonZeroU32::new(1).unwrap(),
            source_generation: 0,
            width: NonZeroU32::new(1).unwrap(),
            height: NonZeroU32::new(1).unwrap(),
            logical_bytes: 4,
            physical_bytes: prepared.physical,
            backing: if KIND == 2 {
                BackingKind::DarwinFixedPosix
            } else {
                BackingKind::LinuxSealed
            },
            digest: prepared.digest,
        },
    )
    .unwrap()
    .map()
    .unwrap();
    let mut actual = [0; 4];
    source.copy_chunk(0, &mut actual).unwrap();
    assert_eq!(actual, [255, 0, 0, 128]);
    assert_eq!(
        PreparedImage::prepare_cancellable(&pixels, &AtomicBool::new(true))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::Interrupted
    );
}
