//! SPIKE (spike/present-cost): where the full-frame write time goes.
//!
//! `cargo run --release -p prismattyc-host --example premultiply_bench`
//!
//! For a 3528x1764 straight-ARGB frame: plain copy, today's scalar
//! copy-then-premultiply, and a copy that skips premultiply for opaque runs
//! (checking alpha 8 pixels at a time). Opaque and 50%-translucent inputs.

#[path = "../src/pixel_alpha.rs"]
mod pixel_alpha;

use std::time::Instant;

use pixel_alpha::premultiply_in_place;

const W: usize = 3528;
const H: usize = 1764;
const ITERS: usize = 60;

fn main() {
    for (label, alpha) in [("opaque", 0xffu32), ("translucent", 0x80)] {
        let src: Vec<u32> = (0..W * H)
            .map(|i| (alpha << 24) | (i as u32 & 0x00ff_ffff))
            .collect();
        let mut dst = vec![0u32; W * H];
        report(label, "copy", time(|| dst.copy_from_slice(&src)));
        report(
            label,
            "copy+premultiply",
            time(|| {
                dst.copy_from_slice(&src);
                premultiply_in_place(&mut dst);
            }),
        );
        report(
            label,
            "copy+opaque-skip",
            time(|| copy_premultiply_fast(&src, &mut dst)),
        );
        report(
            label,
            "copy+branchless",
            time(|| copy_premultiply_branchless(&src, &mut dst)),
        );
        let mut check = vec![0u32; W * H];
        copy_premultiply_branchless(&src, &mut check);
        let mut want = src.clone();
        premultiply_in_place(&mut want);
        copy_premultiply_fast(&src, &mut dst);
        assert_eq!(dst, want, "fast path must match premultiply_in_place");
        assert_eq!(
            check, want,
            "branchless path must match premultiply_in_place"
        );
    }
}

/// Copy, and premultiply only 8-pixel chunks that contain a non-opaque pixel.
fn copy_premultiply_fast(src: &[u32], dst: &mut [u32]) {
    dst.copy_from_slice(src);
    for chunk in dst.chunks_mut(8) {
        let opaque = chunk.iter().fold(0xffu32, |acc, px| acc & (px >> 24)) == 0xff;
        if !opaque {
            premultiply_in_place(chunk);
        }
    }
}

/// Same arithmetic as `premultiply_in_place` (`c * a / 255`), without the
/// opaque branch, so LLVM can vectorize the loop.
fn copy_premultiply_branchless(src: &[u32], dst: &mut [u32]) {
    for (out, &px) in dst.iter_mut().zip(src) {
        let a = px >> 24;
        let r = ((px >> 16) & 0xff) * a / 255;
        let g = ((px >> 8) & 0xff) * a / 255;
        let b = (px & 0xff) * a / 255;
        *out = (a << 24) | (r << 16) | (g << 8) | b;
    }
}

fn time(mut f: impl FnMut()) -> Vec<u64> {
    (0..ITERS)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed().as_micros() as u64
        })
        .collect()
}

fn report(label: &str, op: &str, mut samples: Vec<u64>) {
    samples.sort_unstable();
    println!(
        "{label:<12} {op:<18} p50_us={:>6} p95_us={:>6} ({:.1} Mpx)",
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        (W * H) as f64 / 1e6
    );
}
