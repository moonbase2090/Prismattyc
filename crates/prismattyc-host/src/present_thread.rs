//! SPIKE (spike/render-thread): Core Animation present off the main thread.
//!
//! The main thread still rasterizes into `MacPresent`'s retained framebuffer.
//! It copies the damaged tiles into a latest-wins mailbox and returns. This
//! thread premultiplies the tiles, wraps them as CGImages, and commits an
//! explicit CATransaction. Tile sublayers are standalone CALayers (not
//! view-backed), which Core Animation allows to change on any thread inside
//! an explicit transaction. Layer geometry (rebuild, resize, scale) stays on
//! main, which first waits for this thread to go idle.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use objc2::rc::Retained;
use objc2_core_graphics::CGColorSpace;
use objc2_quartz_core::{CALayer, CATransaction};

use crate::pixel_alpha::premultiply_in_place;

/// Straight ARGB pixels of one damaged tile.
pub struct TilePixels {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

/// Retained tile layers handed to the present thread. CALayer property
/// changes inside an explicit transaction are thread-safe; objc2 does not
/// mark CALayer Send, so this wrapper asserts it.
struct Layers(Vec<Retained<CALayer>>);
// SAFETY: see the type comment; retain/release are atomic.
unsafe impl Send for Layers {}
// SAFETY: only the present thread reads layers; main waits for idle first.
unsafe impl Sync for Layers {}

#[derive(Default)]
struct Pending {
    /// Latest pixels per tile index. A newer frame replaces an unsent tile,
    /// so a slow commit coalesces frames instead of queueing them.
    tiles: BTreeMap<usize, TilePixels>,
    layers: Option<Arc<Layers>>,
    busy: bool,
    shutdown: bool,
}

struct Mailbox {
    state: Mutex<Pending>,
    changed: Condvar,
}

pub struct PresentThread {
    mailbox: Arc<Mailbox>,
    handle: Option<JoinHandle<()>>,
}

impl PresentThread {
    pub fn spawn() -> Self {
        let mailbox = Arc::new(Mailbox {
            state: Mutex::new(Pending::default()),
            changed: Condvar::new(),
        });
        let worker = mailbox.clone();
        let handle = std::thread::Builder::new()
            .name("prismattyc-present".into())
            .spawn(move || run(&worker))
            .expect("spawn present thread");
        Self {
            mailbox,
            handle: Some(handle),
        }
    }

    /// Replace the layer set after main rebuilt the tile grid. Pending tiles
    /// from the old grid are dropped; main resubmits a full frame.
    pub fn set_layers(&self, layers: Vec<Retained<CALayer>>) {
        self.wait_idle();
        let mut state = self.mailbox.state.lock().unwrap();
        state.tiles.clear();
        state.layers = Some(Arc::new(Layers(layers)));
    }

    /// Merge this frame's damaged tiles into the mailbox and wake the thread.
    pub fn submit(&self, tiles: Vec<(usize, TilePixels)>) {
        let mut state = self.mailbox.state.lock().unwrap();
        let mut replaced = 0;
        for (index, tile) in tiles {
            replaced += usize::from(state.tiles.insert(index, tile).is_some());
        }
        crate::spike_timing::value("present_thread.coalesced_tiles", replaced as u64);
        drop(state);
        self.mailbox.changed.notify_all();
    }

    /// Block until nothing is pending or in flight.
    pub fn wait_idle(&self) {
        let started = Instant::now();
        let mut state = self.mailbox.state.lock().unwrap();
        while state.busy || !state.tiles.is_empty() {
            state = self.mailbox.changed.wait(state).unwrap();
        }
        crate::spike_timing::record("present_thread.wait_idle", started.elapsed());
    }
}

impl Drop for PresentThread {
    fn drop(&mut self) {
        self.mailbox.state.lock().unwrap().shutdown = true;
        self.mailbox.changed.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(mailbox: &Mailbox) {
    let Some(color_space) = CGColorSpace::new_device_rgb() else {
        eprintln!("prismattyc-host: present thread has no RGB color space");
        return;
    };
    loop {
        let (tiles, layers) = {
            let mut state = mailbox.state.lock().unwrap();
            while state.tiles.is_empty() && !state.shutdown {
                state = mailbox.changed.wait(state).unwrap();
            }
            if state.shutdown {
                return;
            }
            state.busy = true;
            (std::mem::take(&mut state.tiles), state.layers.clone())
        };
        if let Some(layers) = layers {
            present(&layers.0, tiles, &color_space);
        }
        mailbox.state.lock().unwrap().busy = false;
        mailbox.changed.notify_all();
    }
}

fn present(layers: &[Retained<CALayer>], tiles: BTreeMap<usize, TilePixels>, space: &CGColorSpace) {
    let started = Instant::now();
    let mut images = Vec::with_capacity(tiles.len());
    for (index, mut tile) in tiles {
        premultiply_in_place(&mut tile.pixels);
        match crate::mac_present::alpha_image(&tile.pixels, tile.width, tile.height, space) {
            Ok(image) => images.push((index, image)),
            Err(error) => eprintln!("prismattyc-host: present thread image: {error:#}"),
        }
    }
    crate::spike_timing::record("present_thread.tile_images", started.elapsed());
    let started = Instant::now();
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    for (index, image) in images {
        if let Some(layer) = layers.get(index) {
            // SAFETY: CALayer retains the immutable CGImage.
            unsafe { layer.setContents(Some(image.as_ref())) };
        }
    }
    CATransaction::commit();
    crate::spike_timing::record("present_thread.ca_commit", started.elapsed());
}
