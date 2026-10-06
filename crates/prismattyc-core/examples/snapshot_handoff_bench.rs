//! SPIKE (spike/render-thread): cost of handing a cell-grid snapshot from
//! the main (parse) thread to a render thread.
//!
//! `cargo run --release -p prismattyc-core --example snapshot_handoff_bench --locked`
//!
//! Strategies, each timed on the producing thread per frame:
//! - `copy`: allocate a fresh `Vec<Cell>` of the whole grid and send it.
//! - `pool`: copy the whole grid into a recycled buffer (two-buffer pool).
//! - `double`: two persistent buffers; copy only rows damaged in this or the
//!   previous frame into the back buffer, then swap under a mutex.
//! - `arc`: rows are `Arc<[Cell]>`; rebuild only damaged rows, share the rest,
//!   and publish a new `Arc<Frame>` under a mutex.
//!
//! `handoff` measures the cross-thread wake latency of one channel send.

use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use prismattyc_core::Cell;

const ITERS: usize = 2000;

fn main() {
    println!("cell_bytes={}", std::mem::size_of::<Cell>());
    for (cols, rows) in [(200usize, 60usize), (400, 120)] {
        let grid = make_grid(cols, rows);
        let bytes = cols * rows * std::mem::size_of::<Cell>();
        println!("\n## {cols}x{rows} ({} KiB)", bytes / 1024);
        println!(
            "{:<10} {:>8} {:>10} {:>10}",
            "strategy", "damaged", "p50_us", "p95_us"
        );
        report("copy", "all", bench_copy(&grid));
        report("pool", "all", bench_pool(&grid));
        for damaged in [1, 5, rows] {
            let label = if damaged == rows {
                "all".to_string()
            } else {
                damaged.to_string()
            };
            report("double", &label, bench_double(&grid, cols, rows, damaged));
            report("arc", &label, bench_arc(&grid, cols, rows, damaged));
        }
    }
    println!();
    report("handoff", "-", bench_handoff());
}

fn make_grid(cols: usize, rows: usize) -> Vec<Cell> {
    (0..cols * rows)
        .map(|i| {
            let mut cell = Cell::default();
            cell.character = char::from(b'a' + (i % 26) as u8);
            cell
        })
        .collect()
}

fn report(name: &str, damaged: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let pick = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    println!(
        "{name:<10} {damaged:>8} {:>10.1} {:>10.1}",
        pick(0.5).as_secs_f64() * 1e6,
        pick(0.95).as_secs_f64() * 1e6
    );
}

/// Run `consume` on a render thread so frees and reads happen off the
/// producer, as in the real design.
fn with_consumer<T: Send + 'static>(
    consume: impl Fn(T) + Send + 'static,
) -> (mpsc::SyncSender<T>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel::<T>(2);
    let handle = std::thread::spawn(move || {
        for item in rx {
            consume(item);
        }
    });
    (tx, handle)
}

fn bench_copy(grid: &[Cell]) -> Vec<Duration> {
    let (tx, handle) = with_consumer(|frame: Vec<Cell>| {
        std::hint::black_box(frame[0]);
    });
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let started = Instant::now();
        let frame = grid.to_vec();
        samples.push(started.elapsed());
        tx.send(frame).unwrap();
    }
    drop(tx);
    handle.join().unwrap();
    samples
}

fn bench_pool(grid: &[Cell]) -> Vec<Duration> {
    let (back_tx, back_rx) = mpsc::sync_channel::<Vec<Cell>>(2);
    back_tx.send(grid.to_vec()).unwrap();
    back_tx.send(grid.to_vec()).unwrap();
    let (tx, handle) = with_consumer(move |frame: Vec<Cell>| {
        std::hint::black_box(frame[0]);
        let _ = back_tx.send(frame);
    });
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let mut frame = back_rx.recv().unwrap();
        let started = Instant::now();
        frame.copy_from_slice(grid);
        samples.push(started.elapsed());
        tx.send(frame).unwrap();
    }
    drop(tx);
    handle.join().unwrap();
    samples
}

struct DoubleBuffer {
    front: Vec<Cell>,
    generation: u64,
}

fn bench_double(grid: &[Cell], cols: usize, rows: usize, damaged: usize) -> Vec<Duration> {
    let shared = Arc::new(Mutex::new(DoubleBuffer {
        front: grid.to_vec(),
        generation: 0,
    }));
    let mut back = grid.to_vec();
    let reader = shared.clone();
    let (tx, handle) = with_consumer(move |_: ()| {
        let front = reader.lock().unwrap();
        std::hint::black_box(front.front[0]);
    });
    let mut prior: Vec<usize> = Vec::new();
    let mut samples = Vec::with_capacity(ITERS);
    for frame in 0..ITERS {
        let now: Vec<usize> = (0..damaged).map(|i| (frame * 7 + i) % rows).collect();
        let started = Instant::now();
        // The back buffer is one frame stale: refresh this frame's rows and
        // last frame's rows.
        for &row in now.iter().chain(prior.iter()) {
            let span = row * cols..(row + 1) * cols;
            back[span.clone()].copy_from_slice(&grid[span]);
        }
        {
            let mut front = shared.lock().unwrap();
            std::mem::swap(&mut front.front, &mut back);
            front.generation += 1;
        }
        samples.push(started.elapsed());
        prior = now;
        tx.send(()).unwrap();
    }
    drop(tx);
    handle.join().unwrap();
    samples
}

struct ArcFrame {
    rows: Vec<Arc<[Cell]>>,
}

fn bench_arc(grid: &[Cell], cols: usize, rows: usize, damaged: usize) -> Vec<Duration> {
    let mut current = Arc::new(ArcFrame {
        rows: grid.chunks(cols).map(Arc::from).collect(),
    });
    let published = Arc::new(Mutex::new(current.clone()));
    let reader = published.clone();
    let (tx, handle) = with_consumer(move |_: ()| {
        let frame = reader.lock().unwrap().clone();
        std::hint::black_box(frame.rows[0][0]);
    });
    let mut samples = Vec::with_capacity(ITERS);
    for frame in 0..ITERS {
        let started = Instant::now();
        let mut next: Vec<Arc<[Cell]>> = current.rows.clone();
        for i in 0..damaged {
            let row = (frame * 7 + i) % rows;
            next[row] = Arc::from(&grid[row * cols..(row + 1) * cols]);
        }
        current = Arc::new(ArcFrame { rows: next });
        *published.lock().unwrap() = current.clone();
        samples.push(started.elapsed());
        tx.send(()).unwrap();
    }
    drop(tx);
    handle.join().unwrap();
    samples
}

/// Producer send to consumer wake, measured by the consumer.
fn bench_handoff() -> Vec<Duration> {
    let (tx, rx) = mpsc::sync_channel::<Instant>(1);
    let (done_tx, done_rx) = mpsc::channel::<Duration>();
    let handle = std::thread::spawn(move || {
        for sent in rx {
            done_tx.send(sent.elapsed()).unwrap();
        }
    });
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        // Let the consumer park, as a render thread between frames would.
        std::thread::sleep(Duration::from_micros(200));
        tx.send(Instant::now()).unwrap();
        samples.push(done_rx.recv().unwrap());
    }
    drop(tx);
    handle.join().unwrap();
    samples
}
