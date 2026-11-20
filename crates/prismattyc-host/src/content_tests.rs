// SPDX-License-Identifier: MIT
use super::*;

fn retained() -> Screen {
    let mut screen = Screen::new(4, 2, 10);
    for _ in 0..3 {
        for character in "OLD".chars() {
            screen.put_char(character);
        }
        screen.carriage_return();
        screen.line_feed();
    }
    screen
}

#[test]
fn classic_reads_keep_history_cursor_and_find_navigation() {
    let screen = retained();
    let content = Content::select(
        &screen,
        999,
        #[cfg(native_transport)]
        None,
    );
    assert!(!content.is_native());
    assert_eq!(
        (content.columns(), content.rows(), content.max_scroll()),
        (4, 2, 2)
    );
    assert_eq!(content.scroll(), 2);
    assert_eq!(content.cursor(), Some(Cursor { row: 1, column: 0 }));
    assert_eq!(content.row_text(0), "OLD");
    assert_eq!(content.abs_row_at_view(0), 0);
    let (hit, rank, scroll) = content.find("old", None, false).unwrap();
    assert_eq!(
        hit,
        HistoryMatch {
            abs_row: 0,
            start_col: 0,
            end_col: 2
        }
    );
    assert_eq!((rank, scroll), ((1, 3), 2));
    assert_eq!(
        content.extract_text(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 2
        }),
        "OLD"
    );
    assert!(content.find("missing", None, false).is_none());
    assert_eq!(content.word_columns(0, 1), Some((0, 2)));
    assert_eq!(content.word_columns(9, 0), None);
    assert_eq!(content.absolute_cursor(), Cursor { row: 3, column: 0 });
    assert_eq!(
        content.viewport_range(),
        Some(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 1,
            end_col: 3,
        })
    );
}

#[cfg(native_transport)]
fn native_scene(cluster: Option<&str>) -> std::sync::Arc<crate::native::scene::NativeScene> {
    native_scene_attributes(cluster, 0x2000, 0x46f, true)
}

#[cfg(native_transport)]
fn native_scene_attributes(
    cluster: Option<&str>,
    lead_meta: u32,
    tail_meta: u32,
    cursor: bool,
) -> std::sync::Arc<crate::native::scene::NativeScene> {
    native_scene_pair(
        cluster,
        lead_meta,
        (0x8000_00ff, 0x80ff_0000, tail_meta),
        cursor,
    )
}

#[cfg(native_transport)]
fn native_scene_pair(
    cluster: Option<&str>,
    lead_meta: u32,
    tail: (u32, u32, u32),
    cursor: bool,
) -> std::sync::Arc<crate::native::scene::NativeScene> {
    use crate::native::frame::literals;
    let mut bytes = literals::FRAME_INITIAL.to_vec();
    bytes.resize(160, 0);
    for (offset, value) in [
        (0, 160),
        (24, 2),
        (32, 2),
        (44, 160),
        (52, 160),
        (60, 160),
        (64, 0),
        (68, 0),
        (72, u32::from(cursor)),
        (128, u32::from('N')),
        (140, lead_meta),
        (144, 0x0011_0000),
        (148, tail.0),
        (152, tail.1),
        (156, tail.2),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    if let Some(text) = cluster {
        let used = (176 + text.len() + 7) & !7;
        bytes.resize(used, 0);
        for (offset, value) in [
            (0, used as u32),
            (40, 1),
            (44, 160),
            (48, text.len() as u32),
            (52, 176),
            (60, used as u32),
            (128, 0x8000_0000),
            (164, text.len() as u32),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[168] = 2;
        bytes[176..176 + text.len()].copy_from_slice(text.as_bytes());
    }
    install_frame(&bytes, 2, 1)
}

#[cfg(native_transport)]
fn install_frame(
    bytes: &[u8],
    columns: u32,
    rows: u32,
) -> std::sync::Arc<crate::native::scene::NativeScene> {
    use crate::native::{
        pool::ImagePool,
        ring::RingContext,
        scene::{tests::Producer, SessionFrames},
        wire12::{MetricsEpoch, PtyEpoch, SessionId},
    };
    let mut frames = SessionFrames::new(
        RingContext {
            session: SessionId::new(1).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: MetricsEpoch::new(9).unwrap(),
            columns,
            rows,
        },
        ImagePool::isolated(),
    )
    .unwrap();
    let producer = Producer::new(frames.descriptor().try_clone_to_owned().unwrap());
    producer.publish(1, bytes);
    assert!(frames.prepare_next().unwrap());
    assert!(frames.install_next().unwrap().is_some());
    frames.installed().unwrap().clone()
}

#[cfg(native_transport)]
#[test]
fn native_reads_use_the_owned_scene_and_preserve_declared_width_without_ansi_history() {
    let screen = retained();
    let scene = native_scene(None);
    let content = Content::select(&screen, 999, Some(&scene));
    assert!(content.is_native());
    assert_eq!(
        (content.columns(), content.rows(), content.max_scroll()),
        (2, 1, 0)
    );
    assert_eq!(content.scroll(), 0);
    assert_eq!(content.cursor(), Some(Cursor { row: 0, column: 0 }));
    assert_eq!(content.row_text(0), "N");
    assert_eq!(content.row_text(1), "");
    assert_eq!(content.abs_row_at_view(0), 0);
    assert_eq!(content.cell(0, 0).unwrap().width(), 2);
    assert_eq!(content.cell(0, 1).unwrap().width(), 0);
    assert!(content.cell(1, 0).is_none());
    assert_eq!(
        content.extract_text(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1
        }),
        "N"
    );
    assert_eq!(
        content.find("n", None, false),
        Some((
            HistoryMatch {
                abs_row: 0,
                start_col: 0,
                end_col: 1
            },
            (1, 1),
            0
        ))
    );
    assert!(content.find("old", None, false).is_none());
    assert_eq!(screen.history_line_text(0), "OLD");
    assert_eq!(
        content.find(
            "n",
            Some(HistoryMatch {
                abs_row: 0,
                start_col: 0,
                end_col: 1
            }),
            true
        ),
        Some((
            HistoryMatch {
                abs_row: 0,
                start_col: 0,
                end_col: 1
            },
            (1, 1),
            0
        ))
    );
}

#[cfg(native_transport)]
#[test]
fn native_copy_find_and_accessibility_read_the_same_full_cluster() {
    const CLUSTER: &str = "e\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}";
    let screen = retained();
    let scene = native_scene(Some(CLUSTER));
    let content = Content::select(&screen, 999, Some(&scene));
    let mut selection = prismattyc_core::Selection::default();
    let mut scroll = 999;
    let mut find = crate::FindMode {
        active: true,
        query: "e".into(),
        last: None,
        rank: None,
    };
    crate::apply_find_step(&mut find, &mut selection, &mut scroll, content, false);
    assert_eq!(
        selection.range(),
        Some(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1
        })
    );
    assert_eq!(find.rank, Some((1, 1)));
    assert_eq!(scroll, 0);
    assert_eq!(
        crate::selected_clipboard_text(&selection, content).as_deref(),
        Some(CLUSTER)
    );
    let document = crate::document_for_content(content, &selection);
    assert_eq!(document.text, CLUSTER);
    assert_eq!(document.text.chars().count(), 21);
    assert_eq!(
        (document.caret, document.sel_anchor, document.sel_focus),
        (0, Some(0), Some(21))
    );
    assert_eq!(screen.history_line_text(0), "OLD");
}

#[cfg(native_transport)]
#[test]
fn native_selection_uses_declared_grid_and_selects_a_wide_word_from_its_tail() {
    let screen = retained();
    let scene = native_scene(None);
    let content = Content::select(&screen, 2, Some(&scene));
    assert_eq!(content.word_columns(0, 0), Some((0, 1)));
    assert_eq!(content.word_columns(0, 1), Some((0, 1)));
    assert_eq!(content.word_columns(1, 0), None);
    assert_eq!(
        content.viewport_range(),
        Some(CellRange {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 1,
        })
    );
    assert_eq!(content.absolute_cursor(), Cursor { row: 0, column: 0 });
    let mut selection = prismattyc_core::Selection::default();
    for motion in [
        winit::keyboard::NamedKey::End,
        winit::keyboard::NamedKey::ArrowRight,
        winit::keyboard::NamedKey::PageDown,
    ] {
        assert!(crate::extend_selection_keyboard(
            &mut selection,
            content,
            motion
        ));
        assert_eq!(
            selection.range(),
            Some(CellRange {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 1,
            })
        );
    }
    assert_eq!(
        crate::selected_clipboard_text(&selection, content).as_deref(),
        Some("N")
    );
    assert_eq!(screen.history_line_text(0), "OLD");
}

#[cfg(native_transport)]
fn url_scene(url: &str) -> std::sync::Arc<crate::native::scene::NativeScene> {
    let columns = url.len() + 2;
    let used = 128 + columns * 16;
    let mut bytes = crate::native::frame::literals::FRAME_INITIAL.to_vec();
    bytes.resize(used, 0);
    for (offset, value) in [
        (0, used as u32),
        (24, columns as u32),
        (32, columns as u32),
        (44, used as u32),
        (52, used as u32),
        (60, used as u32),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    let glyphs = url.chars().enumerate().flat_map(|(index, character)| {
        let wide = index == 0 || index + 1 == url.len();
        std::iter::once((u32::from(character), if wide { 0x2000u32 } else { 0x1000 }))
            .chain(wide.then_some((0x11_0000, 0)))
    });
    for (column, (glyph, meta)) in glyphs.enumerate() {
        let start = 128 + column * 16;
        for (offset, value) in [(0, glyph), (4, 0x80ff_ffff), (8, 0x8000_0000), (12, meta)] {
            bytes[start + offset..start + offset + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
    install_frame(&bytes, columns as u32, 1)
}

#[cfg(native_transport)]
#[test]
fn native_links_and_hover_read_current_scene_and_include_wide_continuations() {
    use crate::{hyperlink, link_click::Identity};
    let mut emulator = prismattyc_emulator::Emulator::new(64, 2, 0);
    emulator.feed(b"\x1b]8;;https://retained.example\x1b\\ANSI\x1b]8;;\x1b\\");
    let screen = emulator.screen();
    let first = url_scene("https://native.example/");
    let content = Content::select(screen, 999, Some(&first));
    let target = hyperlink::target_at(content, 0, 1).unwrap();
    assert_eq!(target.url, "https://native.example/");
    assert_eq!(
        target.identity,
        Identity::Detected {
            start: (0, 0),
            end: (0, 24)
        }
    );
    assert!(hyperlink::target_at(content, 1, 1).is_none());
    assert_eq!(
        hyperlink::target_at(content, 0, 24).unwrap().url,
        "https://native.example/"
    );
    assert!(hyperlink::target_at(content, 0, 25).is_none());
    let domain = prismattyc_mux::Domain::bootstrap("content").unwrap();
    let window = domain.sessions().next().unwrap().windows[0];
    let pane = domain.window(window).unwrap().layout.panes()[0];
    let mut hover = hyperlink::HoverPreview::default();
    assert!(hover.update(pane, Content::select(screen, 0, None), 0, 1));
    assert_eq!(hover.url(), Some("https://retained.example"));
    assert!(hover.update(pane, content, 0, 1));
    assert_eq!(hover.url(), Some("https://native.example/"));
    assert!(!hover.update(pane, content, 0, 1));
    // This is a new owner/frame with the same epoch and sequence, not a new
    // retained-screen epoch. Its hover still has to resolve again.
    let second = url_scene("https://newest.example/");
    assert!(hover.update(pane, Content::select(screen, 0, Some(&second)), 0, 1));
    assert_eq!(hover.url(), Some("https://newest.example/"));
    assert!(hover.update(pane, Content::select(screen, 0, None), 0, 1));
    assert_eq!(hover.url(), Some("https://retained.example"));
}

#[cfg(native_transport)]
#[test]
fn native_raster_uses_lead_style_for_mismatched_pairs_and_clips_to_guest() {
    use crate::raster::{self, native_raster};
    let scene = native_scene_attributes(Some(" "), 0x2000, 0, false);
    assert!(!scene.blinks());
    assert!(
        !native_scene_attributes(Some(" "), 0x2040, 0, false).blinks(),
        "blank clusters need no timer"
    );
    assert!(
        !native_scene(None).blinks(),
        "continuation styling is ignored"
    );
    assert!(
        !native_scene_attributes(None, 0x2140, 0, false).blinks(),
        "hidden ink needs no timer"
    );
    let font = raster::FontMetrics::load(16.0).unwrap();
    let stride = font.cell_w * 2 + 4;
    let height = font.cell_h + 4;
    let mut pixels = vec![0x1122_3344; stride * height];
    native_raster::rasterize(
        native_raster::Paint {
            scene: &scene,
            theme: crate::theme::default_theme(),
            font: &font,
            selection: None,
            cursor: false,
            blink_on: true,
            background: raster::ScreenPaint::default(),
        },
        native_raster::Target {
            buffer: &mut pixels,
            stride,
            rect: [2, 2, font.cell_w * 2, font.cell_h],
        },
    );
    for y in 0..height {
        for x in 0..stride {
            let expected = if y >= 2 && y < 2 + font.cell_h && x >= 2 && x < 2 + 2 * font.cell_w {
                0xff00_0000
            } else {
                0x1122_3344
            };
            assert_eq!(pixels[y * stride + x], expected, "pixel ({x},{y})");
        }
    }
}

#[cfg(native_transport)]
fn image_frames(
    pixels: &[u8],
    width: u32,
) -> (
    crate::native::scene::SessionFrames,
    crate::native::scene::tests::Producer,
) {
    use crate::native::{
        backing::{self, tests as backing_fixture},
        pool::ImagePool,
        ring::RingContext,
        scene::{tests::Producer, SessionFrames},
        wire12::{MetricsEpoch, PtyEpoch, Reply, SessionId},
    };
    use sha2::Digest;
    let mut frames = SessionFrames::new(
        RingContext {
            session: SessionId::new(1).unwrap(),
            epoch: PtyEpoch::new(7).unwrap(),
            metrics: MetricsEpoch::new(9).unwrap(),
            columns: 1,
            rows: 1,
        },
        ImagePool::isolated(),
    )
    .unwrap();
    let producer = Producer::new(frames.descriptor().try_clone_to_owned().unwrap());
    producer.publish(1, crate::native::frame::literals::FRAME_INITIAL);
    assert!(frames.prepare_next().unwrap());
    assert!(frames.install_next().unwrap().is_some());
    let (descriptor, physical) = backing_fixture::source_pixels(pixels);
    let mut metadata = backing_fixture::metadata(physical);
    metadata.width = std::num::NonZeroU32::new(width).unwrap();
    metadata.logical_bytes = pixels.len() as u64;
    metadata.digest = sha2::Sha256::digest(pixels).into();
    assert!(matches!(
        frames
            .put(
                backing::ImageDescriptor::validate(descriptor, metadata).unwrap(),
                std::time::Instant::now()
            )
            .unwrap(),
        Reply::Admitted { .. }
    ));
    frames.finish_upload_for_test().unwrap();
    (frames, producer)
}

#[cfg(native_transport)]
#[test]
fn native_raster_orders_images_around_ink_and_omission_restores_background() {
    use crate::native::frame::literals;
    use crate::raster::{self, native_raster};
    let (mut frames, producer) = image_frames(&[255, 0, 0, 128], 1);
    let font = raster::FontMetrics::load(20.0).unwrap();
    let mut pixels = vec![0xff00_0000; 8 * 16];
    for (sequence, depth, opacity, glyph, expected) in [
        (2, Some(-1i32), 255u8, u32::from(' '), 0xff80_0000u32),
        (3, Some(-1), 255, 0x2588u32, 0xffff_ffff),
        (4, Some(0), 255, 0x2588, 0xffff_7f7f),
        (5, Some(0), 128, 0x2588, 0xffff_bfbf),
        (6, None, 255, u32::from(' '), 0xff00_0000),
    ] {
        let mut bytes = if depth.is_some() {
            literals::FRAME_PLACEMENT
        } else {
            literals::FRAME_OMISSION
        }
        .to_vec();
        bytes[128..132].copy_from_slice(&glyph.to_le_bytes());
        if let Some(depth) = depth {
            bytes[148..152].copy_from_slice(&depth.to_le_bytes());
            bytes[192] = opacity;
        }
        producer.publish(sequence, &bytes);
        assert!(frames.prepare_next().unwrap());
        assert!(frames.install_next().unwrap().is_some());
        native_raster::rasterize(
            native_raster::Paint {
                scene: frames.installed().unwrap(),
                theme: crate::theme::default_theme(),
                font: &font,
                selection: None,
                cursor: false,
                blink_on: true,
                background: raster::ScreenPaint::default(),
            },
            native_raster::Target {
                buffer: &mut pixels,
                stride: 8,
                rect: [0, 0, 8, 16],
            },
        );
        assert!(
            pixels.iter().all(|&pixel| pixel == expected),
            "sequence {sequence}: {pixels:x?}"
        );
    }
}

#[cfg(native_transport)]
#[test]
fn native_raster_samples_the_original_image_destination_after_guest_clipping() {
    use crate::raster::{self, native_raster};
    let (mut frames, producer) = image_frames(
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ],
        4,
    );
    let mut bytes = crate::native::frame::literals::FRAME_PLACEMENT.to_vec();
    for (offset, value) in [
        (128, u32::from(' ')),
        (168, 4),
        (176, (-2i32) as u32),
        (184, 4),
        (188, 1),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    producer.publish(2, &bytes);
    assert!(frames.prepare_next().unwrap());
    assert!(frames.install_next().unwrap().is_some());
    let font = raster::FontMetrics::load(16.0).unwrap();
    let mut pixels = vec![0x1122_3344; 6 * 3];
    native_raster::rasterize(
        native_raster::Paint {
            scene: frames.installed().unwrap(),
            theme: crate::theme::default_theme(),
            font: &font,
            selection: None,
            cursor: false,
            blink_on: true,
            background: raster::ScreenPaint::default(),
        },
        native_raster::Target {
            buffer: &mut pixels,
            stride: 6,
            rect: [2, 1, 2, 1],
        },
    );
    assert_eq!(
        pixels,
        [
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0xff00_00ff,
            0xffff_ff00,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
            0x1122_3344,
        ]
    );
}

#[cfg(native_transport)]
#[test]
fn native_raster_honors_text_attributes_and_native_cursor_metadata() {
    use crate::raster::{self, native_raster};
    let font = raster::FontMetrics::load_baked(16.0).unwrap();
    let width = 2 * font.cell_w;
    let height = font.cell_h;
    let mut theme = crate::theme::default_theme().clone();
    theme.cursor_fg = Some([0, 0, 0]);
    theme.cursor_bg = Some([0, 255, 0]);
    let render = |text: &str, meta, blink_on, cursor| {
        let scene = native_scene_attributes(Some(text), meta, 0, cursor);
        let mut pixels = vec![0; width * height];
        native_raster::rasterize(
            native_raster::Paint {
                scene: &scene,
                theme: &theme,
                font: &font,
                selection: None,
                cursor,
                blink_on,
                background: raster::ScreenPaint::default(),
            },
            native_raster::Target {
                buffer: &mut pixels,
                stride: width,
                rect: [0, 0, width, height],
            },
        );
        pixels
    };
    for (meta, blink_on, expected) in [
        (0x2002, true, 0xff7f_7f7f),
        (0x2080, true, 0xff00_0000),
        (0x2040, true, 0xffff_ffff),
    ] {
        assert!(
            render("\u{2588}", meta, blink_on, false)
                .iter()
                .all(|&pixel| pixel == expected),
            "meta {meta:x}"
        );
    }
    for (meta, blink_on) in [(0x2100, true), (0x2040, false)] {
        let pixels = render("\u{2588}", meta, blink_on, false);
        assert!(pixels.chunks_exact(width).all(|row| row[..font.cell_w]
            .iter()
            .all(|&pixel| pixel == 0xff00_0000)
            && row[font.cell_w..].iter().all(|&pixel| pixel == 0xff00_0000)));
    }
    let at = |pixels: &[u32], x, y| pixels[y * width + x];
    let single = render(" ", 0x2008, true, false);
    assert_eq!(at(&single, 0, height - 2), 0xffff_ffff);
    assert_eq!(at(&single, 0, height - 1), 0xff00_0000);
    let double = render(" ", 0x2010, true, false);
    assert_eq!(
        [
            at(&double, 0, height - 3),
            at(&double, 0, height - 2),
            at(&double, 0, height - 1)
        ],
        [0xffff_ffff, 0xff00_0000, 0xffff_ffff]
    );
    let curly = render(" ", 0x2018, true, false);
    assert_eq!(
        [
            at(&curly, 0, height - 3),
            at(&curly, 1, height - 2),
            at(&curly, 2, height - 1),
            at(&curly, 3, height - 2)
        ],
        [0xffff_ffff; 4]
    );
    assert_eq!(
        [at(&curly, 0, height - 2), at(&curly, 1, height - 1)],
        [0xff00_0000; 2]
    );
    let dotted = render(" ", 0x2020, true, false);
    assert_eq!(
        (0..4)
            .map(|x| at(&dotted, x, height - 2))
            .collect::<Vec<_>>(),
        [0xffff_ffff, 0xff00_0000, 0xffff_ffff, 0xff00_0000]
    );
    let dashed = render(" ", 0x2028, true, false);
    assert_eq!(
        (0..6)
            .map(|x| at(&dashed, x, height - 2))
            .collect::<Vec<_>>(),
        [
            0xffff_ffff,
            0xffff_ffff,
            0xffff_ffff,
            0xff00_0000,
            0xff00_0000,
            0xffff_ffff
        ]
    );
    let strike = render(" ", 0x2200, true, false);
    assert_eq!(
        [at(&strike, 0, height / 2), at(&strike, 0, 0)],
        [0xffff_ffff, 0xff00_0000]
    );
    let overline = render(" ", 0x2400, true, false);
    assert_eq!(
        [at(&overline, 0, 0), at(&overline, 0, height / 2)],
        [0xffff_ffff, 0xff00_0000]
    );
    let normal = render("|", 0x2000, true, false);
    let bold = render("|", 0x2001, true, false);
    assert!(bold
        .iter()
        .zip(&normal)
        .all(|(&bold, &normal)| (bold & 0xff) >= (normal & 0xff)));
    assert!(
        bold.iter()
            .zip(&normal)
            .any(|(&bold, &normal)| (bold & 0xff) > (normal & 0xff)),
        "bold adds ink"
    );
    let italic = render("\u{2588}", 0x2004, true, false);
    assert_eq!(at(&italic, 0, 0), 0xff00_0000);
    assert_eq!(at(&italic, font.cell_w, 0), 0xffff_ffff);
    let cursor = render(" ", 0x2000, true, true);
    assert!(cursor.chunks_exact(width).all(|row| row[..font.cell_w]
        .iter()
        .all(|&pixel| pixel == 0xff00_ff00)
        && row[font.cell_w..].iter().all(|&pixel| pixel == 0xff00_0000)));
}

#[cfg(native_transport)]
#[test]
fn native_mismatched_wide_pair_renders_like_a_pair_with_the_lead_style_in_both_cells() {
    use crate::raster::{self, native_raster};
    let font = raster::FontMetrics::load_baked(16.0).unwrap();
    let (width, height) = (2 * font.cell_w, font.cell_h);
    let render = |scene: &crate::native::scene::NativeScene, blink_on| {
        let mut pixels = vec![0x1234_5678; width * height];
        native_raster::rasterize(
            native_raster::Paint {
                scene,
                theme: crate::theme::default_theme(),
                font: &font,
                selection: None,
                cursor: false,
                blink_on,
                background: raster::ScreenPaint::default(),
            },
            native_raster::Target {
                buffer: &mut pixels,
                stride: width,
                rect: [0, 0, width, height],
            },
        );
        pixels
    };
    for lead_meta in [0x2000, 0x2002, 0x2080, 0x2100, 0x2048, 0x2400, 0x2005] {
        let mismatch = native_scene_pair(
            Some("N"),
            lead_meta,
            (0x8000_00ff, 0x80ff_0000, 0x46f),
            false,
        );
        let matching = native_scene_pair(
            Some("N"),
            lead_meta,
            (0x80ff_ffff, 0x8000_0000, lead_meta & 0x7ff),
            false,
        );
        assert_eq!(mismatch.blinks(), matching.blinks());
        for blink_on in [false, true] {
            assert_eq!(
                render(&mismatch, blink_on),
                render(&matching, blink_on),
                "lead {lead_meta:x}, blink {blink_on}"
            );
        }
    }
    let blank = native_scene_pair(Some(" "), 0x2000, (0x8000_00ff, 0x80ff_0000, 0x46f), false);
    assert!(!blank.blinks());
    assert!(render(&blank, true)
        .iter()
        .all(|&pixel| pixel == 0xff00_0000));
    let hidden = native_scene_pair(Some("N"), 0x2100, (0x8000_00ff, 0x80ff_0000, 0x46f), false);
    assert!(!hidden.blinks());
    assert!(render(&hidden, true)
        .iter()
        .all(|&pixel| pixel == 0xff00_0000));
}
