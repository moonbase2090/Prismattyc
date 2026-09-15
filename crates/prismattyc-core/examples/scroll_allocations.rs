//! Count allocation churn after history reaches capacity. Run in a separate process.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
struct Counter;
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(size, Ordering::Relaxed);
        System.realloc(ptr, old, size)
    }
}
#[global_allocator]
static COUNT: Counter = Counter;
fn main() {
    for history in [0, 10_000] {
        for alt in [false, true] {
            let mut screen = prismattyc_core::Screen::new(80, 24, history);
            if alt {
                screen.enter_alt_screen(prismattyc_core::AltScreenMode::Mode1049);
            }
            for _ in 0..10_032 {
                screen.line_feed();
            }
            let start = ALLOCS.load(Ordering::Relaxed);
            let bytes = BYTES.load(Ordering::Relaxed);
            for _ in 0..1000 {
                screen.line_feed();
            }
            println!(
                "history={history} alt={alt} allocations={} bytes={}",
                ALLOCS.load(Ordering::Relaxed) - start,
                BYTES.load(Ordering::Relaxed) - bytes
            );
            std::hint::black_box(screen);
        }
    }
}
