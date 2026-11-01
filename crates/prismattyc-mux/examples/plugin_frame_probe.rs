// SPDX-License-Identifier: MIT
//! Inspect real component frames without terminal escape encoding.

use anyhow::{bail, Context, Result};
use pmux_plugin_api::host::pmux::view::frame;
use prismattyc_mux::plugins::{decode_frame, Client, Reply, Request};

fn main() -> Result<()> {
    let socket = std::env::args_os()
        .nth(1)
        .context("usage: plugin_frame_probe SOCKET")?;
    let mut client = Client::connect(std::path::Path::new(&socket))?;
    let viewport = frame::Viewport {
        columns: 100,
        rows: 24,
        pixels: Some(frame::PixelSize {
            width: 8,
            height: 16,
        }),
    };
    let Reply::ViewOpened { handle } = client.request(Request::OpenView {
        endpoints: None,
        id: "org.moonbase.activity-monitor".into(),
        contribution: "main".into(),
        viewport: viewport.clone(),
        preferences: frame::Preferences {
            motion: false,
            snapshot: true,
            ..Default::default()
        },
    })?
    else {
        bail!("expected view handle");
    };
    let reply = client.request(Request::ViewEvent {
        handle: handle.clone(),
        event: frame::Event::Tick(frame::Clock {
            wall_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            monotonic_ms: 0,
        }),
    })?;
    let Reply::Frame {
        handle: returned,
        content,
        ..
    } = reply
    else {
        bail!("expected typed frame");
    };
    anyhow::ensure!(returned == handle, "view handle mismatch");
    let content = decode_frame(&content, &viewport)?;
    let failure_pixels = content
        .content()
        .tiles
        .iter()
        .map(|tile| {
            tile.rgba
                .chunks_exact(4)
                .filter(|pixel| pixel.starts_with(&[255, 122, 107, 255]))
                .count()
        })
        .sum::<usize>();
    let rasters = content.content().tiles.iter().map(|tile| serde_json::json!({"width":tile.pixel_width,"height":tile.pixel_height,"bytes":tile.rgba.len()})).collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!({"text":content.text(),"cells":content.content().cells.len(),"rasters":rasters,"failure_pixels":failure_pixels})
    );
    client.request(Request::CloseView { handle })?;
    Ok(())
}
