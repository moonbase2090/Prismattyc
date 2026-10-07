// SPDX-License-Identifier: MPL-2.0
//! Compare the previous per-pixel conversion with `pixel_alpha`'s chunked path.
//!
//! Run with `cargo run --release -p prismattyc-host --example premultiply_bench`.

#[path = "../src/pixel_alpha.rs"]
mod pixel_alpha;

use std::hint::black_box;
use std::time::Instant;

use pixel_alpha::premultiply_in_place;

const WIDTH: usize = 3528;
const HEIGHT: usize = 1764;
const WARMUP: usize = 8;
const ITERATIONS: usize = 60;
const MAX_ACCEPTED_AFTER_PERCENT: u128 = 90;

fn main() {
    let acceptance = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--acceptance") => true,
        Some(argument) => panic!("unknown argument {argument:?}; expected --acceptance"),
    };

    for (label, alpha) in [("opaque", 0xffu32), ("alpha=128", 0x80u32)] {
        let source: Vec<u32> = (0..WIDTH * HEIGHT)
            .map(|index| alpha << 24 | index as u32 & 0x00ff_ffff)
            .collect();
        let mut expected = source.clone();
        baseline_premultiply_in_place(&mut expected);
        let mut actual = source.clone();
        premultiply_in_place(&mut actual);
        assert_eq!(actual, expected, "optimized pixels must match baseline");

        let mut destination = vec![0u32; source.len()];
        let (before, after) = measure(&source, &mut destination);
        let before_p50 = percentile(&before, 50);
        let after_p50 = percentile(&after, 50);
        println!(
            "{label:<10} before p50={:.3} ms p95={:.3} ms | after p50={:.3} ms p95={:.3} ms",
            before_p50 as f64 / 1_000_000.0,
            percentile(&before, 95) as f64 / 1_000_000.0,
            after_p50 as f64 / 1_000_000.0,
            percentile(&after, 95) as f64 / 1_000_000.0,
        );
        if acceptance {
            assert!(
                after_p50 * 100 <= before_p50 * MAX_ACCEPTED_AFTER_PERCENT,
                "native pixel-alpha acceptance failed for {label}: optimized p50 {:.3} ms must be at most {}% of reference p50 {:.3} ms",
                after_p50 as f64 / 1_000_000.0,
                MAX_ACCEPTED_AFTER_PERCENT,
                before_p50 as f64 / 1_000_000.0,
            );
            println!(
                "PASS {label}: optimized p50 is at most {MAX_ACCEPTED_AFTER_PERCENT}% of the reference"
            );
        }
    }
    println!(
        "{WIDTH}x{HEIGHT} pixels, {ITERATIONS} alternating samples after {WARMUP} warmups per path"
    );
}

fn measure(source: &[u32], destination: &mut [u32]) -> (Vec<u128>, Vec<u128>) {
    for _ in 0..WARMUP {
        sample(source, destination, baseline_premultiply_in_place);
        sample(source, destination, premultiply_in_place);
    }

    let mut before = Vec::with_capacity(ITERATIONS);
    let mut after = Vec::with_capacity(ITERATIONS);
    for index in 0..ITERATIONS {
        let (first, second) = if index % 2 == 0 {
            (
                sample(source, destination, baseline_premultiply_in_place),
                sample(source, destination, premultiply_in_place),
            )
        } else {
            (
                sample(source, destination, premultiply_in_place),
                sample(source, destination, baseline_premultiply_in_place),
            )
        };
        if index % 2 == 0 {
            before.push(first);
            after.push(second);
        } else {
            after.push(first);
            before.push(second);
        }
    }
    (before, after)
}

fn sample(source: &[u32], destination: &mut [u32], convert: fn(&mut [u32])) -> u128 {
    destination.copy_from_slice(source);
    let started = Instant::now();
    convert(destination);
    black_box(&*destination);
    started.elapsed().as_nanos()
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() * percentile / 100]
}

fn baseline_premultiply_in_place(buffer: &mut [u32]) {
    for pixel in buffer.iter_mut() {
        let alpha = *pixel >> 24;
        if alpha == 255 {
            continue;
        }
        let red = ((*pixel >> 16) & 0xff) * alpha / 255;
        let green = ((*pixel >> 8) & 0xff) * alpha / 255;
        let blue = (*pixel & 0xff) * alpha / 255;
        *pixel = (alpha << 24) | (red << 16) | (green << 8) | blue;
    }
}
