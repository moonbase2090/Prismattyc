//! Native Wayland `wl_shm` present with per-pixel alpha (PT-118).
//!
//! softbuffer 0.4 hardcodes `wl_shm` `Xrgb8888`, so its Wayland path drops
//! the alpha byte. This module presents `Argb8888` buffers on the winit
//! window's own `wl_surface` through a guest handle on the same connection.
//! ARGB8888 is one of the two `wl_shm` formats every compositor must
//! accept, so the path is portable by specification rather than by probing.
//! winit clears the opaque region on `with_transparent(true)` windows
//! (`reload_transparency_hint`), so an ARGB buffer alone gives real
//! show-through; no EGL/Vulkan driver path is involved.
//!
//! Optional blur uses `ext_background_effect_manager_v1` where the
//! compositor advertises it (KWin 6.7+). wlroots compositors such as
//! Hyprland apply blur compositor-side via window rules instead; see
//! `docs/hyprland.md`.

mod buffer_age;

use std::fs::File;
use std::os::unix::io::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use memmap2::MmapMut;
use rustix::event::{PollFd, PollFlags, Timespec};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, ExtBackgroundEffectManagerV1,
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use winit::window::Window;

use self::buffer_age::BufferAge;
use crate::frame_damage::FrameDamage;

/// Is this window on a native Wayland connection (not X11/XWayland)?
pub fn is_wayland(window: &Window) -> bool {
    let Ok(handle) = window.window_handle() else {
        return false;
    };
    matches!(handle.as_raw(), RawWindowHandle::Wayland(_))
}

/// Buffers in rotation before `prepare()` reuses one the compositor still
/// holds. Compositors hold at most two in steady state (scanout + queued).
const MAX_BUFFERS: usize = 4;
/// Total budget for waiting on a buffer release. An occluded window (other
/// workspace, lock screen) gets no releases; painting must never park the
/// winit event loop on that, or the compositor marks the window as not
/// responding.
const RELEASE_WAIT: Duration = Duration::from_millis(200);

/// CPU present over `wl_shm` ARGB8888. The caller rasterizes straight-alpha
/// `0xAARRGGBB` into the canonical framebuffer returned by `pixels_mut()`.
/// `present()` repairs and premultiplies only the pixels required by the age
/// of the selected rotating shm slot.
pub struct WaylandShmPresent {
    state: ShmState,
    queue: EventQueue<ShmState>,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    buffers: Vec<WaylandBuffer>,
    /// Stable straight-alpha raster target shared by every rotating slot.
    framebuffer: BufferAge,
    /// Index of the buffer attached by the last `present()`.
    front: usize,
    /// Index prepared by `prepare()`; written until `present()`.
    back: usize,
    /// The front-buffer reuse fallback logs once, not per frame.
    logged_reuse: bool,
    /// Keeps the blur effect object alive; `None` when blur is off or the
    /// compositor has no background-effect protocol.
    blur: Option<ExtBackgroundEffectSurfaceV1>,
}

#[derive(Default)]
struct ShmState {
    blur_capabilities: u32,
}

impl WaylandShmPresent {
    /// Wrap the winit window's surface with our own ARGB8888 double buffer.
    ///
    /// `want_blur` requests a compositor background blur for the whole
    /// surface when the protocol and the capability exist. Failure to set up
    /// blur is not fatal; check `blur_active()`.
    pub fn try_init(window: &Window, want_blur: bool) -> Result<Self> {
        let display_ptr = match window.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(handle)) => handle.display.as_ptr(),
            _ => return Err(anyhow!("not a Wayland display")),
        };
        let surface_ptr = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Wayland(handle)) => handle.surface.as_ptr(),
            _ => return Err(anyhow!("not a Wayland window")),
        };

        // Guest backend on winit's connection: never closes the display on
        // drop. winit keeps winit's objects; we only add our own.
        let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) = registry_queue_init::<ShmState>(&conn)
            .context("wayland registry on the winit connection")?;
        let qh = queue.handle();
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=2, ())
            .context("wl_shm is not advertised")?;

        // winit's wl_surface, wrapped without taking ownership: the proxy is
        // used for requests only, and wayland-client proxies send no
        // destructor on drop.
        let surface_id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface_ptr.cast()) }
                .map_err(|_| anyhow!("winit surface is not a wl_surface"))?;
        let surface = wl_surface::WlSurface::from_id(&conn, surface_id)
            .map_err(|_| anyhow!("cannot wrap the winit wl_surface"))?;

        let size = window.inner_size();
        let width = size.width.max(1) as i32;
        let height = size.height.max(1) as i32;
        let buffers = vec![
            WaylandBuffer::new(&shm, width, height, &qh)?,
            WaylandBuffer::new(&shm, width, height, &qh)?,
        ];

        let mut state = ShmState::default();
        let blur = if want_blur {
            setup_blur(&globals, &qh, &conn, &mut queue, &mut state, &surface)
        } else {
            None
        };

        Ok(Self {
            state,
            queue,
            shm,
            surface,
            buffers,
            framebuffer: BufferAge::new(width as usize, height as usize),
            front: 0,
            back: 1,
            logged_reuse: false,
            blur,
        })
    }

    /// Did a compositor background blur get installed for this surface?
    pub fn blur_active(&self) -> bool {
        self.blur.is_some()
    }

    /// Pick the back buffer for the next frame: a released one, a fresh one,
    /// or — bounded — the front buffer again. Never parks the event loop:
    /// an occluded window gets no releases and must stay responsive.
    pub fn prepare(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;
        if self.framebuffer.resize(width as usize, height as usize) {
            for buffer in &mut self.buffers {
                buffer.generation = None;
            }
        }
        self.pump_events();
        if let Some(back) = self.take_released(width, height)? {
            self.back = back;
            return Ok(());
        }
        if self.buffers.len() < MAX_BUFFERS {
            let qh = self.queue.handle();
            self.buffers
                .push(WaylandBuffer::new(&self.shm, width, height, &qh)?);
            self.back = self.buffers.len() - 1;
            return Ok(());
        }
        let deadline = Instant::now() + RELEASE_WAIT;
        while Instant::now() < deadline {
            if !self.wait_release(deadline) {
                break;
            }
            if let Some(back) = self.take_released(width, height)? {
                self.back = back;
                return Ok(());
            }
        }
        if !self.logged_reuse {
            self.logged_reuse = true;
            eprintln!(
                "prismattyc-host: wayland shm: compositor holds all {MAX_BUFFERS} buffers; \
                 reusing the front buffer (tearing is possible while the window is occluded)"
            );
        }
        self.back = self.front;
        Ok(())
    }

    /// First released buffer, resized to the frame if needed.
    fn take_released(&mut self, width: i32, height: i32) -> Result<Option<usize>> {
        let Some(index) = self.buffers.iter().position(|b| b.released()) else {
            return Ok(None);
        };
        self.buffers[index].resize(width, height)?;
        Ok(Some(index))
    }

    /// Read and dispatch pending events without blocking, so `released`
    /// flags track the compositor between paints.
    fn pump_events(&mut self) {
        let _ = self.queue.dispatch_pending(&mut self.state);
        let Some(guard) = self.queue.prepare_read() else {
            return;
        };
        let fd = guard.connection_fd();
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        let idle = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let readable = rustix::event::poll(&mut fds, Some(&idle))
            .map(|n| n > 0)
            .unwrap_or(false);
        if readable && guard.read().is_ok() {
            let _ = self.queue.dispatch_pending(&mut self.state);
        }
    }

    /// Block up to `deadline` for socket traffic, then dispatch. Returns
    /// false on timeout or error, true when events were dispatched.
    fn wait_release(&mut self, deadline: Instant) -> bool {
        let Some(guard) = self.queue.prepare_read() else {
            return false;
        };
        let fd = guard.connection_fd();
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(n) if n > 0 => {
                if guard.read().is_err() {
                    return false;
                }
                self.queue.dispatch_pending(&mut self.state).is_ok()
            }
            _ => false,
        }
    }

    /// Canonical frame pixels, `0xAARRGGBB` straight alpha.
    /// Call only between `prepare()` and `present()`.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.framebuffer.pixels_mut()
    }

    /// Repair the selected shm slot for its age, attach it, and commit.
    pub fn present(&mut self, damage: FrameDamage) -> Result<()> {
        let slot_generation = self.buffers[self.back].generation;
        let generation = {
            // The pool only grows and stays mapped for the buffer's lifetime,
            // so the mapping cannot be invalidated under the repair pass.
            let target = unsafe { self.buffers[self.back].mapped_mut() };
            self.framebuffer
                .repair_slot(slot_generation, damage, target)
        };
        self.buffers[self.back].generation = Some(generation);
        self.buffers[self.back].attach(&self.surface);
        // Keep compositor damage full-surface for now. CPU-side buffer repair
        // above is independent from surface-coordinate damage, especially at
        // fractional scale factors. PT-290 provides isolated compositor proof
        // before that optimization is attempted.
        // `damage` (v1) is used instead of `damage_buffer` (v4) because the
        // surface version belongs to winit's bind, not to this proxy.
        self.surface.damage(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
        self.queue.flush().context("wayland flush")?;
        self.front = self.back;
        Ok(())
    }
}

/// Bind `ext_background_effect_manager_v1` and blur the whole surface.
/// Returns the live effect object, or `None` when the protocol is absent or
/// the compositor reports no blur capability.
fn setup_blur(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<ShmState>,
    conn: &Connection,
    queue: &mut EventQueue<ShmState>,
    state: &mut ShmState,
    surface: &wl_surface::WlSurface,
) -> Option<ExtBackgroundEffectSurfaceV1> {
    let manager: ExtBackgroundEffectManagerV1 = globals.bind(qh, 1..=1, ()).ok()?;
    // The capabilities event arrives after the bind; one roundtrip collects
    // it. Startup-only, so the block is bounded by the compositor.
    queue.roundtrip(state).ok()?;
    if state.blur_capabilities & ext_background_effect_manager_v1::Capability::Blur.bits() == 0 {
        return None;
    }
    let effect = manager.get_background_effect(surface, qh, ());
    // A NULL region removes the effect, so whole-surface blur needs an
    // explicit region; it is clipped to the surface by the compositor.
    let compositor: wl_compositor::WlCompositor = globals.bind(qh, 1..=4, ()).ok()?;
    let region = compositor.create_region(qh, ());
    region.add(0, 0, i32::MAX, i32::MAX);
    effect.set_blur_region(Some(&region));
    region.destroy();
    surface.commit();
    conn.flush().ok()?;
    Some(effect)
}

struct WaylandBuffer {
    qh: QueueHandle<ShmState>,
    tempfile: File,
    map: MmapMut,
    pool: wl_shm_pool::WlShmPool,
    pool_size: i32,
    buffer: wl_buffer::WlBuffer,
    width: i32,
    height: i32,
    /// Canonical framebuffer generation currently copied into this slot.
    generation: Option<u64>,
    released: Arc<AtomicBool>,
}

impl WaylandBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        width: i32,
        height: i32,
        qh: &QueueHandle<ShmState>,
    ) -> Result<Self> {
        let pool_size = pool_size_for(width, height);
        let tempfile = create_memfile();
        tempfile
            .set_len(pool_size as u64)
            .context("size the wl_shm pool memfd")?;
        let map = unsafe { map_file(&tempfile) };
        let pool = shm.create_pool(tempfile.as_fd(), pool_size, qh, ());
        let released = Arc::new(AtomicBool::new(true));
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            qh,
            released.clone(),
        );
        Ok(Self {
            qh: qh.clone(),
            tempfile,
            map,
            pool,
            pool_size,
            buffer,
            width,
            height,
            generation: None,
            released,
        })
    }

    fn resize(&mut self, width: i32, height: i32) -> Result<()> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        let size = pool_size_for(width, height);
        if size > self.pool_size {
            // Grow the file before touching pool or buffer: on failure the
            // old mapping stays valid and prepare() reports the error
            // instead of the next raster write SIGBUSing on a short file.
            self.tempfile
                .set_len(size as u64)
                .context("grow the wl_shm pool memfd")?;
            self.buffer.destroy();
            self.pool.resize(size);
            self.pool_size = size;
            self.map = unsafe { map_file(&self.tempfile) };
        } else {
            self.buffer.destroy();
        }
        self.buffer = self.pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &self.qh,
            self.released.clone(),
        );
        self.width = width;
        self.height = height;
        self.generation = None;
        Ok(())
    }

    fn attach(&self, surface: &wl_surface::WlSurface) {
        self.released.store(false, Ordering::SeqCst);
        surface.attach(Some(&self.buffer), 0, 0);
    }

    fn released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    unsafe fn mapped_mut(&mut self) -> &mut [u32] {
        let len = self.width as usize * self.height as usize;
        unsafe { std::slice::from_raw_parts_mut(self.map.as_mut_ptr().cast::<u32>(), len) }
    }
}

impl Drop for WaylandBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// Round the pool to a power of two so resizes rarely remap.
fn pool_size_for(width: i32, height: i32) -> i32 {
    ((width * height * 4) as u32).next_power_of_two() as i32
}

fn create_memfile() -> File {
    use rustix::fs::{MemfdFlags, SealFlags};
    let name = c"prismattyc-host";
    let fd = rustix::fs::memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .expect("memfd_create for the wl_shm pool");
    rustix::fs::fcntl_add_seals(&fd, SealFlags::SHRINK | SealFlags::SEAL)
        .expect("seal the wl_shm pool memfd");
    File::from(fd)
}

unsafe fn map_file(file: &File) -> MmapMut {
    unsafe { MmapMut::map_mut(file.as_fd().as_raw_fd()).expect("map the wl_shm pool") }
}

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for ShmState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for ShmState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for ShmState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for ShmState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        released: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            released.store(true, Ordering::SeqCst);
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for ShmState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for ShmState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for ShmState {
    fn event(
        state: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities {
            flags: wayland_client::WEnum::Value(flags),
        } = event
        {
            state.blur_capabilities = flags.bits();
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for ShmState {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectSurfaceV1,
        _: wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_rounds_up_to_power_of_two() {
        assert_eq!(pool_size_for(1, 1), 4);
        assert_eq!(pool_size_for(80, 24), 8192);
        assert_eq!(pool_size_for(1920, 1080), 8_388_608);
    }
}
