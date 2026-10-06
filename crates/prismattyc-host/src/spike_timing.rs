//! SPIKE ONLY (spike/render-thread): per-phase wall-clock timing.
//!
//! `PRISMATTYC_SPIKE_TIMING=<path>` enables it. Every two seconds the
//! cumulative per-phase table (count, total, p50, p95, p99, max) is rewritten
//! to `<path>`. Creating `<path>.reset` clears the samples, so a workload
//! script can drop warm-up. Disabled, every call is one relaxed atomic load.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<Mutex<Recorder>> = OnceLock::new();

struct Recorder {
    path: PathBuf,
    samples: BTreeMap<&'static str, Vec<u32>>,
    started: Instant,
    last_flush: Instant,
}

/// Read the environment once at startup.
pub fn init() {
    let Some(path) = std::env::var_os("PRISMATTYC_SPIKE_TIMING") else {
        return;
    };
    let now = Instant::now();
    let _ = STATE.set(Mutex::new(Recorder {
        path: PathBuf::from(path),
        samples: BTreeMap::new(),
        started: now,
        last_flush: now,
    }));
    ENABLED.store(true, Ordering::Relaxed);
}

/// `PRISMATTYC_SPIKE_CELLS=COLSxROWS` sets the first window's cell size.
pub fn initial_cells() -> Option<(usize, usize)> {
    let raw = std::env::var("PRISMATTYC_SPIKE_CELLS").ok()?;
    let (cols, rows) = raw.split_once('x')?;
    Some((cols.parse().ok()?, rows.parse().ok()?))
}

/// `PRISMATTYC_SPIKE_POLL_DRAIN=1`: re-arm leftover drain with Poll, not Wake.
pub fn poll_drain() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PRISMATTYC_SPIKE_POLL_DRAIN").is_some())
}

/// `PRISMATTYC_SPIKE_DRAIN_BUDGET_MS=N`: drain up to N ms per pump.
pub fn drain_budget() -> Option<Duration> {
    static BUDGET: OnceLock<Option<Duration>> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let ms = std::env::var("PRISMATTYC_SPIKE_DRAIN_BUDGET_MS").ok()?;
        Some(Duration::from_millis(ms.parse().ok()?))
    })
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Time `f` under `name`.
pub fn time<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let started = Instant::now();
    let out = f();
    record(name, started.elapsed());
    out
}

pub fn record(name: &'static str, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let Some(state) = STATE.get() else { return };
    let mut state = state.lock().unwrap();
    let micros = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX);
    state.samples.entry(name).or_default().push(micros);
}

/// Record a raw value (not a duration) under `name`, e.g. a count or size.
pub fn value(name: &'static str, value: u64) {
    record(name, Duration::from_micros(value));
}

/// Count one event under a runtime label (interned; spike only).
pub fn count(prefix: &str, label: &str) {
    if !enabled() {
        return;
    }
    static NAMES: OnceLock<Mutex<BTreeMap<String, &'static str>>> = OnceLock::new();
    let key = format!("{prefix}.{label}");
    let name = *NAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key.clone())
        .or_insert_with(|| Box::leak(key.into_boxed_str()));
    record(name, Duration::ZERO);
}

/// Rewrite the report when two seconds have passed; honor `<path>.reset`.
pub fn maybe_flush() {
    if !enabled() {
        return;
    }
    let Some(state) = STATE.get() else { return };
    let mut state = state.lock().unwrap();
    let reset = state.path.with_extension("reset");
    if reset.exists() {
        let _ = std::fs::remove_file(&reset);
        state.samples.clear();
        state.started = Instant::now();
    }
    if state.last_flush.elapsed() < Duration::from_secs(2) {
        return;
    }
    state.last_flush = Instant::now();
    let _ = std::fs::write(&state.path, report(&state));
}

fn report(state: &Recorder) -> String {
    let window = state.started.elapsed().as_secs_f64();
    let mut out = format!(
        "window_s={window:.1}\n{:<34} {:>8} {:>10} {:>8} {:>8} {:>8} {:>8}\n",
        "phase", "count", "total_ms", "p50_us", "p95_us", "p99_us", "max_us"
    );
    for (name, samples) in &state.samples {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
        let total: u64 = sorted.iter().map(|&v| u64::from(v)).sum();
        let _ = writeln!(
            out,
            "{:<34} {:>8} {:>10.1} {:>8} {:>8} {:>8} {:>8}",
            name,
            sorted.len(),
            total as f64 / 1000.0,
            pick(0.50),
            pick(0.95),
            pick(0.99),
            sorted[sorted.len() - 1],
        );
    }
    out
}

/// SPIKE (present-cost): marginal 512x128 tiles each damage stage adds.
/// `PRISMATTYC_SPIKE_DAMAGE_STAGES=1` enables it (macOS tile grid).
pub struct DamageStages {
    active: bool,
    width: usize,
    height: usize,
    last: usize,
}

impl DamageStages {
    pub fn new(width: u32, height: u32, damage: &crate::frame_damage::FrameDamage) -> Self {
        let active = enabled() && std::env::var_os("PRISMATTYC_SPIKE_DAMAGE_STAGES").is_some();
        let mut stages = Self {
            active,
            width: width as usize,
            height: height as usize,
            last: 0,
        };
        stages.mark("compose", damage);
        stages
    }

    fn tiles(&self, damage: &crate::frame_damage::FrameDamage) -> usize {
        #[cfg(target_os = "macos")]
        {
            let grid = crate::present_tiles::tiles(self.width, self.height);
            crate::present_tiles::damaged_tiles(&grid, damage).len()
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = damage;
            0
        }
    }

    pub fn mark(&mut self, stage: &str, damage: &crate::frame_damage::FrameDamage) {
        if !self.active {
            return;
        }
        let now = self.tiles(damage);
        count_value(
            "damage.added_tiles",
            stage,
            now.saturating_sub(self.last) as u64,
        );
        self.last = now;
    }

    /// Final damage, including pane rows and blits added while painting.
    pub fn finish(&mut self, damage: &crate::frame_damage::FrameDamage) {
        if !self.active {
            return;
        }
        let full = matches!(damage, crate::frame_damage::FrameDamage::Full);
        self.mark(if full { "promoted_full" } else { "panes" }, damage);
        value("damage.final_tiles", self.last as u64);
        if let crate::frame_damage::FrameDamage::Rects(rects) = damage {
            value("damage.rects", rects.len() as u64);
            let area: usize = rects.iter().map(|r| r.width * r.height).sum();
            value(
                "damage.rect_area_pct",
                (area * 100 / (self.width * self.height).max(1)) as u64,
            );
        }
    }
}

/// Record a value under a runtime label (interned; spike only).
pub fn count_value(prefix: &str, label: &str, v: u64) {
    if !enabled() {
        return;
    }
    static NAMES: OnceLock<Mutex<BTreeMap<String, &'static str>>> = OnceLock::new();
    let key = format!("{prefix}.{label}");
    let name = *NAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key.clone())
        .or_insert_with(|| Box::leak(key.into_boxed_str()));
    value(name, v);
}
