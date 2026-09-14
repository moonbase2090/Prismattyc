# Kitty Graphics Decoder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render inline Kitty-graphics images (e.g. the Claude Code logo) in prism-host, by default, on both the direct-host and mux paths.

**Architecture:** A dedicated `_G` APC collector in `prism-protocol` recovers Kitty graphics escapes from the byte stream (VTE swallows APC). A `GraphicsState` on `Emulator` (always constructed, in both `new` and `new_experimental`) parses the control data, answers the capability query, reassembles `m=` chunks, base64-decodes, PNG-decodes with strict bounds, and stores decoded RGBA plus a cell-anchored placement. prism-host reads `Emulator::images()` in `rasterize_frame` and alpha-blits each image, scaled to its cell rect, through the existing `raster::blit_rgba`. prism-core is untouched.

**Tech Stack:** Rust workspace (Cargo). New deps in `prism-emulator`: `png = "0.17"` (already a workspace lock entry via prism-host). Base64 decode is hand-rolled (no new dep, matches the repo's no-dep-CLI style). No `clap`, no serde additions.

**Spec:** `docs/superpowers/specs/2026-08-20-kitty-graphics-decoder-design.md` (read it alongside this plan).

## Global Constraints

- **Activation:** Graphics must work with NO flag. Intake lives in `Emulator` itself and runs in EVERY constructor path, so it is independent of `experimental_rich`. The `--experimental-rich` flag and `new_experimental` are being removed as the rich experience folds in by default; do NOT couple graphics to either — graphics must keep working after that removal with no change. Render path in `rasterize_frame` must not be gated on `pane.experimental_rich()`.
- **prism-core is decoder-free:** `Cell` is `Copy + PartialEq + Eq`; do NOT add image fields to `prism_core::Cell` or `Screen`. Image state lives on `prism_emulator::Emulator`.
- **Fail-closed negotiation:** Never send image data or a partial frame. Only answer the `a=q` query with `ESC _ G i=<id>;OK ESC \`. Do not rely on the `$TERM` allowlist (Prism hides terminal identity on purpose).
- **Bounds before allocation (decompression-bomb defense):** Reject on IHDR dimensions and byte caps BEFORE decoding pixels. Use `png::Limits`. Checked arithmetic on every `w*h*bpp`, stride, offset.
- **Limits (prototype values):** max dimensions 2048×1024 px; max decoded frame 8 MiB; max retained raster per PTY generation 16 MiB; one in-flight `m=` reassembly per image id.
- **Format subset (first cut):** actions `a=T`/`a=q`/`a=d`; format `f=100` (PNG) only; transports `t=d` (inline) and `t=f` (regular file); chunking `m=0`/`m=1`; `q` quietness respected. Reject anything else with a bounded warning and drop; never crash, never block the PTY.
- **Quiet idle preserved:** a static image adds no timer. A new/changed/removed image sets `host.dirty` and requests a redraw through the existing drain path; idle stays `ControlFlow::Wait`.
- **`t=f` file safety:** open then `fstat` the fd (no path re-lookup); require regular file, current-uid owner, size within cap; treat contents as untrusted.
- Run `cargo fmt` and `cargo clippy` clean. Source `$HOME/.cargo/env` before cargo in each shell.

---

## File Structure

**prism-protocol** (`crates/prism-protocol/src/`)
- `graphics_apc.rs` (Create): `GraphicsApcCollector` + `GraphicsApc` — recovers `ESC _ G …;… ESC \`, splits control vs payload, enforces the graphics body cap. Independent of the existing `ApcCollector`.
- `lib.rs` (Modify): `mod graphics_apc; pub use graphics_apc::{GraphicsApcCollector, GraphicsApc};`

**prism-emulator** (`crates/prism-emulator/src/`)
- `Cargo.toml` (Modify): add `png = "0.17"`.
- `graphics/mod.rs` (Create): `GraphicsState`, `PlacedImage`, `GraphicsCommand`, control parsing, `m=` reassembly, `a=q`/`a=T`/`a=d` handling, memory cap.
- `graphics/base64.rs` (Create): `decode(&[u8]) -> Result<Vec<u8>, Base64Error>`.
- `graphics/decode.rs` (Create): `decode_png_bounded(&[u8], MaxDims) -> Result<DecodedImage, DecodeError>`.
- `graphics/file_transport.rs` (Create): `read_file_bounded(path, max_bytes) -> Result<Vec<u8>, FileError>` (safe open+fstat).
- `lib.rs` (Modify): `mod graphics;` add `graphics: graphics::GraphicsState` field to `Emulator`, construct it in `new` and `new_experimental`, drive it in `feed`, add `images()` accessor and invalidation hooks.

**prism-host** (`crates/prism-host/src/`)
- `raster.rs` (Modify): add `blit_rgba_scaled(...)` (nearest-neighbor scale + alpha-blend, reusing the `blit_rgba` blend/clip logic).
- `main.rs` (Modify `rasterize_frame` ~1233-1246): after each pane's screen paint, blit `pane.emulator.images()` into the pane clip box.

**prism-mux** (`crates/prism-mux/src/`) — Stage 3
- `live.rs` / `rich.rs` (Modify): server-owned `Emulator` already holds images; ensure reattach restores them and generation change invalidates.

---

## STAGE 1 — Direct host: `t=d` inline PNG, query reply, render

### Task 1: Base64 decoder

**Files:**
- Create: `crates/prism-emulator/src/graphics/base64.rs`
- Create (stub for module tree): `crates/prism-emulator/src/graphics/mod.rs` with `pub(crate) mod base64;`
- Modify: `crates/prism-emulator/src/lib.rs` — add `mod graphics;` near the other `mod`/`use` (top of file).

**Interfaces:**
- Produces: `pub(crate) fn decode(input: &[u8]) -> Result<Vec<u8>, Base64Error>`; `pub(crate) enum Base64Error { BadChar(u8), BadLength }`. Standard RFC 4648 alphabet, `=` padding accepted, ASCII whitespace ignored.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/base64.rs  (bottom)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_vectors() {
        assert_eq!(decode(b"").unwrap(), b"");
        assert_eq!(decode(b"Zg==").unwrap(), b"f");
        assert_eq!(decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(decode(b"Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn ignores_ascii_whitespace() {
        assert_eq!(decode(b"Zm9v\r\n YmFy").unwrap(), b"foobar");
    }

    #[test]
    fn rejects_bad_char_and_length() {
        assert!(matches!(decode(b"Zm9*"), Err(Base64Error::BadChar(b'*'))));
        assert!(matches!(decode(b"Zm9"), Err(Base64Error::BadLength)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::base64 -- --nocapture`
Expected: FAIL — `decode` / `Base64Error` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/prism-emulator/src/graphics/base64.rs  (top)
//! Minimal RFC 4648 base64 decoder for Kitty graphics payloads.
//! Hand-rolled to avoid a new dependency (matches the no-dep-CLI style).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Base64Error {
    BadChar(u8),
    BadLength,
}

const INVALID: u8 = 0xFF;
const PAD: u8 = 0xFE;

fn value(byte: u8) -> u8 {
    match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        b'=' => PAD,
        _ => INVALID,
    }
}

pub(crate) fn decode(input: &[u8]) -> Result<Vec<u8>, Base64Error> {
    // Collect non-whitespace symbols first (Kitty may wrap payloads).
    let mut syms: Vec<u8> = Vec::with_capacity(input.len());
    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        let v = value(b);
        if v == INVALID {
            return Err(Base64Error::BadChar(b));
        }
        syms.push(v);
    }
    if syms.len() % 4 != 0 {
        return Err(Base64Error::BadLength);
    }
    let mut out = Vec::with_capacity(syms.len() / 4 * 3);
    for chunk in syms.chunks(4) {
        let pads = chunk.iter().filter(|&&v| v == PAD).count();
        // Padding may only appear in the final positions.
        if pads > 2 || (pads > 0 && chunk[0] == PAD) || (pads == 2 && chunk[1] == PAD) {
            return Err(Base64Error::BadLength);
        }
        let b0 = chunk[0];
        let b1 = chunk[1];
        let b2 = if chunk[2] == PAD { 0 } else { chunk[2] };
        let b3 = if chunk[3] == PAD { 0 } else { chunk[3] };
        out.push((b0 << 2) | (b1 >> 4));
        if chunk[2] != PAD {
            out.push((b1 << 4) | (b2 >> 2));
        }
        if chunk[3] != PAD {
            out.push((b2 << 6) | b3);
        }
    }
    Ok(out)
}
```

Add to `crates/prism-emulator/src/graphics/mod.rs`:
```rust
pub(crate) mod base64;
```
Add to `crates/prism-emulator/src/lib.rs` (with the other top-level `mod` declarations):
```rust
mod graphics;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::base64`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/base64.rs crates/prism-emulator/src/graphics/mod.rs crates/prism-emulator/src/lib.rs
git commit -m "feat(emulator): base64 decoder for Kitty graphics payloads"
```

---

### Task 2: `_G` APC collector (prism-protocol)

**Files:**
- Create: `crates/prism-protocol/src/graphics_apc.rs`
- Modify: `crates/prism-protocol/src/lib.rs` — add module + re-export.
- Test: in `graphics_apc.rs`.

**Interfaces:**
- Consumes: nothing (byte-level).
- Produces:
  - `pub struct GraphicsApcCollector { … }` with `pub fn new() -> Self`, `pub fn push(&mut self, bytes: &[u8]) -> Vec<GraphicsApc>`, `pub const fn is_active(&self) -> bool`.
  - `pub struct GraphicsApc { pub control: String, pub payload: Vec<u8> }` — `control` is the text before the first `;` with the leading `G` stripped (e.g. `"a=T,t=d,f=100,q=2"`); `payload` is the raw bytes after the first `;` (still base64). A `_G` body with no `;` yields `control` = whole body, `payload` empty (queries have no payload after `;`… actually Kitty queries DO carry a payload; if no `;`, payload is empty).
- Cap: `MAX_GRAPHICS_APC_BYTES = 5_000_000` total body bytes; on overflow the sequence is dropped (no event) but bytes keep draining to ST to stay in sync.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-protocol/src/graphics_apc.rs  (bottom)
#[cfg(test)]
mod tests {
    use super::*;

    fn one(bytes: &[u8]) -> Vec<GraphicsApc> {
        GraphicsApcCollector::new().push(bytes)
    }

    #[test]
    fn parses_transmit_and_display() {
        let ev = one(b"\x1b_Ga=T,t=d,f=100,q=2;SGk=\x1b\\");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].control, "a=T,t=d,f=100,q=2");
        assert_eq!(ev[0].payload, b"SGk=");
    }

    #[test]
    fn parses_query_with_no_semicolon_payload() {
        // Query: control then ';' then a tiny base64 pixel.
        let ev = one(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].control, "i=31,s=1,v=1,a=q,t=d,f=24");
        assert_eq!(ev[0].payload, b"AAAA");
    }

    #[test]
    fn ignores_non_graphics_apc() {
        // A Prism-namespace APC must NOT be captured by the graphics collector.
        let ev = one(b"\x1b_Prism;cap;foo\x1b\\");
        assert!(ev.is_empty());
    }

    #[test]
    fn reassembles_split_across_pushes() {
        let mut c = GraphicsApcCollector::new();
        assert!(c.push(b"\x1b_Ga=T,t=d,f=100;SG").is_empty());
        assert!(c.is_active());
        let ev = c.push(b"k=\x1b\\");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].payload, b"SGk=");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-protocol graphics_apc`
Expected: FAIL — module/types not found.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/prism-protocol/src/graphics_apc.rs  (top)
//! Sidecar collector for Kitty graphics APC sequences: `ESC _ G <ctrl>;<payload> ESC \`.
//!
//! VTE swallows APC without delivering it to `Perform`, so we scan the raw
//! byte stream in parallel (same technique as `ApcCollector`, but scoped to
//! the `G` graphics namespace, with a much larger body cap for base64 pixels).

/// Total body-byte ceiling for a single graphics sequence. Oversized bodies
/// are dropped (no event) but drained to ST to keep the stream in sync.
pub const MAX_GRAPHICS_APC_BYTES: usize = 5_000_000;

/// A complete Kitty graphics command recovered from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsApc {
    /// Control data before the first `;`, with the leading `G` stripped.
    pub control: String,
    /// Raw (still base64) payload bytes after the first `;`.
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// In an APC body; `is_graphics` set once we confirm the leading `G`.
    Body,
    BodyEsc,
}

#[derive(Debug, Default)]
pub struct GraphicsApcCollector {
    state: State,
    buffer: Vec<u8>,
    seen_first: bool,
    is_graphics: bool,
    overflow: bool,
}

impl GraphicsApcCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn is_active(&self) -> bool {
        !matches!(self.state, State::Ground)
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<GraphicsApc> {
        let mut out = Vec::new();
        for &b in bytes {
            if let Some(ev) = self.push_byte(b) {
                out.push(ev);
            }
        }
        out
    }

    fn reset_body(&mut self) {
        self.buffer.clear();
        self.seen_first = false;
        self.is_graphics = false;
        self.overflow = false;
    }

    fn push_byte(&mut self, byte: u8) -> Option<GraphicsApc> {
        match self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.state = State::Esc;
                }
                None
            }
            State::Esc => {
                if byte == b'_' {
                    self.state = State::Body;
                    self.reset_body();
                } else {
                    self.state = State::Ground;
                }
                None
            }
            State::Body => match byte {
                0x1b => {
                    self.state = State::BodyEsc;
                    None
                }
                _ => {
                    if !self.seen_first {
                        self.seen_first = true;
                        self.is_graphics = byte == b'G';
                        // Do not store the leading 'G'.
                        return None;
                    }
                    if self.is_graphics {
                        if self.buffer.len() >= MAX_GRAPHICS_APC_BYTES {
                            self.overflow = true;
                        } else if !self.overflow {
                            self.buffer.push(byte);
                        }
                    }
                    None
                }
            },
            State::BodyEsc => {
                if byte == b'\\' {
                    self.state = State::Ground;
                    let emit = self.is_graphics && !self.overflow;
                    let body = core::mem::take(&mut self.buffer);
                    self.reset_body();
                    if emit {
                        return Some(split_body(&body));
                    }
                    None
                } else {
                    // Not ST; treat the ESC as spurious and keep collecting.
                    self.state = State::Body;
                    if self.is_graphics && byte != 0x1b {
                        if self.buffer.len() >= MAX_GRAPHICS_APC_BYTES {
                            self.overflow = true;
                        } else if !self.overflow {
                            self.buffer.push(byte);
                        }
                    }
                    None
                }
            }
        }
    }
}

fn split_body(body: &[u8]) -> GraphicsApc {
    match body.iter().position(|&b| b == b';') {
        Some(i) => GraphicsApc {
            control: String::from_utf8_lossy(&body[..i]).into_owned(),
            payload: body[i + 1..].to_vec(),
        },
        None => GraphicsApc {
            control: String::from_utf8_lossy(body).into_owned(),
            payload: Vec::new(),
        },
    }
}
```

Add to `crates/prism-protocol/src/lib.rs` (with the other modules / re-exports):
```rust
mod graphics_apc;
pub use graphics_apc::{GraphicsApc, GraphicsApcCollector, MAX_GRAPHICS_APC_BYTES};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-protocol graphics_apc`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/prism-protocol/src/graphics_apc.rs crates/prism-protocol/src/lib.rs
git commit -m "feat(protocol): Kitty graphics APC collector (ESC _ G ... ST)"
```

---

### Task 3: Control-data parser

**Files:**
- Create: `crates/prism-emulator/src/graphics/command.rs`
- Modify: `crates/prism-emulator/src/graphics/mod.rs` — `pub(crate) mod command;`
- Test: in `command.rs`.

**Interfaces:**
- Consumes: `GraphicsApc.control` (`&str`).
- Produces:
  - `pub(crate) struct GraphicsCommand { pub action: Action, pub format: u16, pub transport: Transport, pub more: bool, pub id: u32, pub cols: u16, pub rows: u16, pub quiet: u8 }`
  - `pub(crate) enum Action { Transmit, Query, Delete, Other }` (`a=T`→Transmit, `a=q`→Query, `a=d`→Delete, else Other)
  - `pub(crate) enum Transport { Direct, File, Other }` (`t=d`→Direct, `t=f`→File, default Direct, else Other)
  - `pub(crate) fn parse(control: &str) -> GraphicsCommand` — unknown keys ignored; defaults: `format=32`, `more=false`, `id=0`, `cols=0`, `rows=0`, `quiet=0`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/command.rs  (bottom)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transmit_display() {
        let c = parse("a=T,t=d,f=100,q=2");
        assert_eq!(c.action, Action::Transmit);
        assert_eq!(c.transport, Transport::Direct);
        assert_eq!(c.format, 100);
        assert_eq!(c.quiet, 2);
        assert!(!c.more);
    }

    #[test]
    fn parses_query_and_file_and_chunk() {
        assert_eq!(parse("i=31,a=q,t=d,f=24").action, Action::Query);
        assert_eq!(parse("a=T,t=f,f=100,i=7").transport, Transport::File);
        assert!(parse("a=T,t=d,f=100,m=1").more);
        assert_eq!(parse("i=42").id, 42);
    }

    #[test]
    fn ignores_unknown_keys_and_defaults() {
        let c = parse("a=T,zz=99");
        assert_eq!(c.format, 32);
        assert_eq!(c.transport, Transport::Direct);
        assert_eq!(c.action, Action::Transmit);
    }

    #[test]
    fn unknown_action_is_other() {
        assert_eq!(parse("a=X").action, Action::Other);
        assert_eq!(parse("t=s").transport, Transport::Other);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::command`
Expected: FAIL — types not found.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/prism-emulator/src/graphics/command.rs  (top)
//! Parse Kitty graphics control data (comma-separated `key=value`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Transmit,
    Query,
    Delete,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Direct,
    File,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GraphicsCommand {
    pub action: Action,
    pub format: u16,
    pub transport: Transport,
    pub more: bool,
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
    pub quiet: u8,
}

pub(crate) fn parse(control: &str) -> GraphicsCommand {
    let mut cmd = GraphicsCommand {
        action: Action::Other,
        format: 32,
        transport: Transport::Direct,
        more: false,
        id: 0,
        cols: 0,
        rows: 0,
        quiet: 0,
    };
    // Kitty omits `a=` for continuation chunks; a bare control still parses.
    let mut saw_action = false;
    for pair in control.split(',') {
        let Some((key, val)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "a" => {
                saw_action = true;
                cmd.action = match val {
                    "T" => Action::Transmit,
                    "q" => Action::Query,
                    "d" => Action::Delete,
                    _ => Action::Other,
                };
            }
            "t" => {
                cmd.transport = match val {
                    "d" => Transport::Direct,
                    "f" => Transport::File,
                    _ => Transport::Other,
                };
            }
            "f" => cmd.format = val.parse().unwrap_or(cmd.format),
            "m" => cmd.more = val == "1",
            "i" => cmd.id = val.parse().unwrap_or(0),
            "c" => cmd.cols = val.parse().unwrap_or(0),
            "r" => cmd.rows = val.parse().unwrap_or(0),
            "q" => cmd.quiet = val.parse().unwrap_or(0),
            _ => {}
        }
    }
    // A continuation chunk (no `a=`) is a Transmit continuation by convention.
    if !saw_action {
        cmd.action = Action::Transmit;
    }
    cmd
}
```

Add to `crates/prism-emulator/src/graphics/mod.rs`:
```rust
pub(crate) mod command;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::command`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/command.rs crates/prism-emulator/src/graphics/mod.rs
git commit -m "feat(emulator): parse Kitty graphics control data"
```

---

### Task 4: PNG decode with strict bounds

**Files:**
- Modify: `crates/prism-emulator/Cargo.toml` — add `png = "0.17"` under `[dependencies]`.
- Create: `crates/prism-emulator/src/graphics/decode.rs`
- Modify: `crates/prism-emulator/src/graphics/mod.rs` — `pub(crate) mod decode;`
- Test: in `decode.rs`.

**Interfaces:**
- Produces:
  - `pub(crate) struct DecodedImage { pub width: u32, pub height: u32, pub rgba: Vec<u8> }` (`rgba.len() == width*height*4`).
  - `pub(crate) struct MaxDims { pub w: u32, pub h: u32, pub bytes: usize }`
  - `pub(crate) enum DecodeError { TooLarge, Format, Corrupt }`
  - `pub(crate) fn decode_png_bounded(bytes: &[u8], max: MaxDims) -> Result<DecodedImage, DecodeError>` — reads IHDR first, rejects `TooLarge` before allocating pixels; sets `png::Limits { bytes: max.bytes }`; normalizes any color type to RGBA8.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/decode.rs  (bottom)
#[cfg(test)]
mod tests {
    use super::*;

    // A 1x1 opaque-red PNG, produced once and pasted as bytes so the test has
    // no encoder dependency. (Generate with: `printf` a real PNG, or the png
    // crate in a scratch bin; the reviewer may regenerate.)
    const RED_1X1_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
        0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00,
        0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
        0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn dims(w: u32, h: u32) -> MaxDims {
        MaxDims { w, h, bytes: 8 << 20 }
    }

    #[test]
    fn decodes_1x1_to_rgba() {
        let img = decode_png_bounded(RED_1X1_PNG, dims(2048, 1024)).unwrap();
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba.len(), 4);
        assert_eq!(img.rgba[0], 0xFF); // red
        assert_eq!(img.rgba[3], 0xFF); // opaque
    }

    #[test]
    fn rejects_when_dimensions_exceed_cap_before_decode() {
        // Cap smaller than the image's 1x1 → TooLarge.
        let err = decode_png_bounded(RED_1X1_PNG, MaxDims { w: 0, h: 0, bytes: 8 << 20 });
        assert!(matches!(err, Err(DecodeError::TooLarge)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            decode_png_bounded(b"not a png", dims(2048, 1024)),
            Err(DecodeError::Format) | Err(DecodeError::Corrupt)
        ));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::decode`
Expected: FAIL — `png` dep / `decode_png_bounded` not found.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/prism-emulator/src/graphics/decode.rs  (top)
//! Bounded PNG decode. Rejects on IHDR dimensions and a byte cap BEFORE
//! allocating pixel buffers (decompression-bomb defense), then normalizes to
//! 8-bit RGBA.

#[derive(Debug, Clone)]
pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MaxDims {
    pub w: u32,
    pub h: u32,
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    TooLarge,
    Format,
    Corrupt,
}

pub(crate) fn decode_png_bounded(bytes: &[u8], max: MaxDims) -> Result<DecodedImage, DecodeError> {
    let mut decoder = png::Decoder::new(bytes);
    // Hard cap on internal allocations (IDAT/palette) — bomb defense.
    decoder.set_limits(png::Limits { bytes: max.bytes });
    // Normalize palette/low-bit-depth to 8-bit channels.
    decoder.set_transformations(png::Transformations::normalize_to_color8());

    let mut reader = decoder.read_info().map_err(map_png_err)?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);

    // Bounds BEFORE allocating the output buffer.
    if w == 0 || h == 0 || w > max.w || h > max.h {
        return Err(DecodeError::TooLarge);
    }
    let pixels = (w as u64)
        .checked_mul(h as u64)
        .ok_or(DecodeError::TooLarge)?;
    let rgba_len = pixels.checked_mul(4).ok_or(DecodeError::TooLarge)?;
    if rgba_len > max.bytes as u64 {
        return Err(DecodeError::TooLarge);
    }

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(map_png_err)?;
    let src = &buf[..frame.buffer_size()];

    let rgba = to_rgba8(src, frame.color_type, w, h)?;
    Ok(DecodedImage { width: w, height: h, rgba })
}

fn to_rgba8(src: &[u8], color: png::ColorType, w: u32, h: u32) -> Result<Vec<u8>, DecodeError> {
    let count = (w as usize).checked_mul(h as usize).ok_or(DecodeError::TooLarge)?;
    let mut out = vec![0u8; count.checked_mul(4).ok_or(DecodeError::TooLarge)?];
    match color {
        png::ColorType::Rgba => {
            if src.len() < count * 4 {
                return Err(DecodeError::Corrupt);
            }
            out.copy_from_slice(&src[..count * 4]);
        }
        png::ColorType::Rgb => {
            if src.len() < count * 3 {
                return Err(DecodeError::Corrupt);
            }
            for i in 0..count {
                out[i * 4] = src[i * 3];
                out[i * 4 + 1] = src[i * 3 + 1];
                out[i * 4 + 2] = src[i * 3 + 2];
                out[i * 4 + 3] = 0xFF;
            }
        }
        png::ColorType::Grayscale => {
            if src.len() < count {
                return Err(DecodeError::Corrupt);
            }
            for i in 0..count {
                let g = src[i];
                out[i * 4] = g;
                out[i * 4 + 1] = g;
                out[i * 4 + 2] = g;
                out[i * 4 + 3] = 0xFF;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if src.len() < count * 2 {
                return Err(DecodeError::Corrupt);
            }
            for i in 0..count {
                let g = src[i * 2];
                out[i * 4] = g;
                out[i * 4 + 1] = g;
                out[i * 4 + 2] = g;
                out[i * 4 + 3] = src[i * 2 + 1];
            }
        }
        png::ColorType::Indexed => return Err(DecodeError::Format), // normalize_to_color8 expands palette
    }
    Ok(out)
}

fn map_png_err(err: png::DecodingError) -> DecodeError {
    match err {
        png::DecodingError::LimitsExceeded => DecodeError::TooLarge,
        png::DecodingError::Format(_) => DecodeError::Format,
        _ => DecodeError::Corrupt,
    }
}
```

Add to `crates/prism-emulator/src/graphics/mod.rs`:
```rust
pub(crate) mod decode;
```
Add to `crates/prism-emulator/Cargo.toml` under `[dependencies]`:
```toml
png = "0.17"
```

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::decode`
Expected: PASS (3 tests). If `normalize_to_color8` is unavailable in the pinned png version, use `Transformations::EXPAND | Transformations::STRIP_16` and keep the `to_rgba8` match.

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/Cargo.toml crates/prism-emulator/src/graphics/decode.rs crates/prism-emulator/src/graphics/mod.rs Cargo.lock
git commit -m "feat(emulator): bounded PNG decode to RGBA8 (bomb-safe)"
```

---

### Task 5: `GraphicsState` — store, query reply, `t=d` transmit

**Files:**
- Create: `crates/prism-emulator/src/graphics/mod.rs` — expand into the state machine (append to the existing module file that currently only holds `mod` lines).
- Test: in `graphics/mod.rs`.

**Interfaces:**
- Consumes: `prism_protocol::GraphicsApc`, `command::parse`, `base64::decode`, `decode::decode_png_bounded`, and the cursor position from `prism_core::Screen` (`screen.cursor()` — a `Cursor { row, column }`; if no public getter exists, add `pub const fn cursor(&self) -> Cursor` to `prism-core` reading `self.active_buffer().cursor`, in a sub-step).
- Produces:
  - `pub struct PlacedImage { pub id: u32, pub rgba: std::sync::Arc<[u8]>, pub width: u32, pub height: u32, pub cols: u16, pub rows: u16, pub anchor_abs_line: u64, pub anchor_col: u16 }`
  - `pub(crate) struct GraphicsState { images: Vec<PlacedImage>, retained_bytes: usize, /* chunk scratch in Task 11 */ }` with:
    - `pub(crate) fn new() -> Self`
    - `pub(crate) fn handle(&mut self, apc: &GraphicsApc, screen: &Screen, replies: &mut Vec<Vec<u8>>, metrics_cols: u16)` — the dispatch entry called from `Emulator::feed`.
    - `pub fn images(&self) -> &[PlacedImage]`
    - `pub(crate) fn clear(&mut self)` (used by invalidation, Task 15)
  - Public re-export from emulator: `pub use graphics::PlacedImage;` in `lib.rs`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/mod.rs  (bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use prism_core::Screen;
    use prism_protocol::GraphicsApc;

    const RED_1X1_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";

    fn apc(control: &str, payload_b64: &str) -> GraphicsApc {
        GraphicsApc { control: control.into(), payload: payload_b64.as_bytes().to_vec() }
    }

    #[test]
    fn query_queues_ok_reply() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("i=31,a=q,t=d,f=24", "AAAA"), &screen, &mut replies, 80);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0], b"\x1b_Gi=31;OK\x1b\\");
        assert!(g.images().is_empty());
    }

    #[test]
    fn query_with_quiet_suppresses_reply() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("i=9,a=q,q=1", "AAAA"), &screen, &mut replies, 80);
        assert!(replies.is_empty());
    }

    #[test]
    fn transmit_display_stores_decoded_image() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("a=T,t=d,f=100,i=5", RED_1X1_PNG_B64), &screen, &mut replies, 80);
        assert_eq!(g.images().len(), 1);
        let img = &g.images()[0];
        assert_eq!(img.id, 5);
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba.len(), 4);
    }

    #[test]
    fn bad_payload_is_dropped_not_panicked() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("a=T,t=d,f=100,i=1", "@@@@"), &screen, &mut replies, 80);
        assert!(g.images().is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::tests`
Expected: FAIL — `GraphicsState` not found.

- [ ] **Step 3: Write minimal implementation**

If `Screen` lacks a public cursor getter, first add to `crates/prism-core/src/lib.rs` (near other `Screen` accessors, e.g. after `scrolled_lines`):
```rust
    /// Current cursor position on the active buffer (row/col, zero-based).
    pub const fn cursor(&self) -> Cursor {
        self.active_buffer_ref().cursor
    }
```
(Use whatever the existing private accessor is named; `GridBuffer.cursor` is the field. Add a matching `const fn active_buffer_ref(&self) -> &GridBuffer` if none exists.)

Then, in `crates/prism-emulator/src/graphics/mod.rs`:
```rust
pub(crate) mod base64;
pub(crate) mod command;
pub(crate) mod decode;

use std::sync::Arc;

use prism_core::Screen;
use prism_protocol::GraphicsApc;

use command::{Action, Transport};
use decode::{decode_png_bounded, MaxDims};

const MAX_W: u32 = 2048;
const MAX_H: u32 = 1024;
const MAX_FRAME_BYTES: usize = 8 << 20; // 8 MiB
const MAX_RETAINED_BYTES: usize = 16 << 20; // 16 MiB per generation
const PNG_FORMAT: u16 = 100;

/// A decoded image placed at a cell anchor.
#[derive(Debug, Clone)]
pub struct PlacedImage {
    pub id: u32,
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub cols: u16,
    pub rows: u16,
    pub anchor_abs_line: u64,
    pub anchor_col: u16,
}

#[derive(Debug, Default)]
pub(crate) struct GraphicsState {
    images: Vec<PlacedImage>,
    retained_bytes: usize,
}

impl GraphicsState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub fn images(&self) -> &[PlacedImage] {
        &self.images
    }

    pub(crate) fn clear(&mut self) {
        self.images.clear();
        self.retained_bytes = 0;
    }

    pub(crate) fn handle(
        &mut self,
        apc: &GraphicsApc,
        screen: &Screen,
        replies: &mut Vec<Vec<u8>>,
        metrics_cols: u16,
    ) {
        let cmd = command::parse(&apc.control);
        match cmd.action {
            Action::Query => {
                if cmd.quiet == 0 {
                    replies.push(format!("\x1b_Gi={};OK\x1b\\", cmd.id).into_bytes());
                }
            }
            Action::Transmit => {
                // First cut: single-chunk t=d only. (t=f in Task 10, m= in Task 11.)
                if cmd.more || cmd.transport != Transport::Direct || cmd.format != PNG_FORMAT {
                    return;
                }
                self.transmit_inline(&cmd, &apc.payload, screen, metrics_cols);
            }
            Action::Delete => { /* Task 14 */ }
            Action::Other => {}
        }
    }

    fn transmit_inline(
        &mut self,
        cmd: &command::GraphicsCommand,
        payload_b64: &[u8],
        screen: &Screen,
        metrics_cols: u16,
    ) {
        let Ok(raw) = base64::decode(payload_b64) else {
            return;
        };
        let max = MaxDims { w: MAX_W, h: MAX_H, bytes: MAX_FRAME_BYTES };
        let Ok(img) = decode_png_bounded(&raw, max) else {
            return;
        };
        self.store(cmd, img, screen, metrics_cols);
    }

    fn store(
        &mut self,
        cmd: &command::GraphicsCommand,
        img: decode::DecodedImage,
        screen: &Screen,
        metrics_cols: u16,
    ) {
        let frame = img.rgba.len();
        if frame > MAX_FRAME_BYTES {
            return;
        }
        // Evict oldest until the new frame fits the per-generation cap.
        while self.retained_bytes + frame > MAX_RETAINED_BYTES && !self.images.is_empty() {
            let dropped = self.images.remove(0);
            self.retained_bytes = self.retained_bytes.saturating_sub(dropped.rgba.len());
        }
        if self.retained_bytes + frame > MAX_RETAINED_BYTES {
            return; // single frame alone exceeds the cap
        }
        let cursor = screen.cursor();
        let anchor_abs_line = screen.scrolled_lines() + cursor.row as u64;
        // Derive cell span if the child did not specify c=/r=.
        let cols = if cmd.cols > 0 { cmd.cols } else { metrics_cols.max(1) };
        let rows = if cmd.rows > 0 { cmd.rows } else { 1 };
        // Same id replaces the prior placement (idempotent commit).
        if let Some(slot) = self.images.iter_mut().find(|p| p.id == cmd.id && cmd.id != 0) {
            self.retained_bytes = self.retained_bytes.saturating_sub(slot.rgba.len());
            slot.rgba = Arc::from(img.rgba.into_boxed_slice());
            slot.width = img.width;
            slot.height = img.height;
            slot.cols = cols;
            slot.rows = rows;
            slot.anchor_abs_line = anchor_abs_line;
            slot.anchor_col = cursor.column as u16;
            self.retained_bytes += frame;
            return;
        }
        self.images.push(PlacedImage {
            id: cmd.id,
            rgba: Arc::from(img.rgba.into_boxed_slice()),
            width: img.width,
            height: img.height,
            cols,
            rows,
            anchor_abs_line,
            anchor_col: cursor.column as u16,
        });
        self.retained_bytes += frame;
    }
}
```
> Note: `store` receives `metrics_cols` as a coarse "how many cells wide is the image" hint (the host recomputes exact pixel scaling from `FontMetrics`; `cols`/`rows` are only the grid footprint for cursor advance and clipping). The first cut may pass a fixed default (e.g. derive from `img.width / typical_cell_px`); refine in the render task.

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::tests`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/mod.rs crates/prism-core/src/lib.rs
git commit -m "feat(emulator): GraphicsState — query reply + t=d PNG store with bounds"
```

---

### Task 6: Wire `GraphicsState` into `Emulator`

**Files:**
- Modify: `crates/prism-emulator/src/lib.rs` — add field, construct in both constructors, drive in `feed`, add `images()` accessor, re-export `PlacedImage`.
- Test: in `crates/prism-emulator/src/lib.rs` `#[cfg(test)]`.

**Interfaces:**
- Consumes: Task 2 collector, Task 5 state.
- Produces: `pub fn images(&self) -> &[graphics::PlacedImage]` on `Emulator`; `pub use graphics::PlacedImage;`. Graphics intake active in EVERY constructor (`new`, and `new_experimental` while it still exists). When `new_experimental` is later removed, the field init stays in `new` and nothing else changes.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/lib.rs  (in the existing #[cfg(test)] mod)
#[test]
fn feed_renders_kitty_query_reply_in_classic_mode() {
    // Classic (non-experimental) emulator must still answer the graphics query.
    let mut emulator = Emulator::new(80, 24, 0);
    let _ = emulator.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    let replies = emulator.take_pending_replies();
    assert!(
        replies.iter().any(|r| r == b"\x1b_Gi=31;OK\x1b\\"),
        "expected graphics OK reply, got {replies:?}"
    );
}

#[test]
fn feed_stores_inline_png_image() {
    const RED_1X1_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    let mut emulator = Emulator::new(80, 24, 0);
    let seq = format!("\x1b_Ga=T,t=d,f=100,i=7;{RED_1X1_PNG_B64}\x1b\\");
    let _ = emulator.feed(seq.as_bytes());
    assert_eq!(emulator.images().len(), 1);
    assert_eq!(emulator.images()[0].id, 7);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator feed_renders_kitty feed_stores_inline`
Expected: FAIL — `images()` not found / no reply.

- [ ] **Step 3: Write minimal implementation**

In `crates/prism-emulator/src/lib.rs`:

1. Add field to `struct Emulator` (after `pending_bell`):
```rust
    /// Kitty graphics decoder state (always active; independent of rich mode).
    graphics: graphics::GraphicsState,
    /// Graphics APC sidecar (independent of the Prism `apc` collector).
    graphics_apc: prism_protocol::GraphicsApcCollector,
```
2. In EVERY `Emulator` constructor (`new`, and `new_experimental` while it exists), add to the struct literal:
```rust
            graphics: graphics::GraphicsState::new(),
            graphics_apc: prism_protocol::GraphicsApcCollector::new(),
```
(These two lines are unconditional — never guarded by an `experimental` flag — so graphics survives the pending removal of `new_experimental`.)
3. In `feed`, BEFORE the existing `self.apc` push (so it runs in classic mode too), collect graphics APCs and handle them after the parser advance. Restructure the head of `feed`:
```rust
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<CollectedApc> {
        let graphics_events = self.graphics_apc.push(bytes);
        let events = match self.apc.as_mut() {
            Some(apc) => apc.push(bytes),
            None => Vec::new(),
        };
        // ... existing destructure + parser.advance(...) unchanged ...
        // (after parser.advance returns)
        let metrics_cols = 0u16; // host recomputes exact scaling; 0 => default span
        for apc in &graphics_events {
            self.graphics
                .handle(apc, &self.screen, &mut self.pending_replies, metrics_cols);
        }
        events
    }
```
> The destructure block borrows `self` fields mutably; call `self.graphics.handle(...)` AFTER that block ends (after `parser.advance`), using `&self.screen` immutably and `&mut self.pending_replies`. Reorder so the destructure's borrow has ended.
4. Add accessor + `apc_pending` extension so the rich chunk splitter also waits on graphics APCs (prevents splitting a graphics sequence mid-flight):
```rust
    pub fn images(&self) -> &[graphics::PlacedImage] {
        self.graphics.images()
    }
```
   And update `apc_pending`:
```rust
    pub fn apc_pending(&self) -> bool {
        self.graphics_apc.is_active()
            || self
                .apc
                .as_ref()
                .is_some_and(prism_protocol::ApcCollector::is_active)
    }
```
5. Add near the top-level re-exports: `pub use graphics::PlacedImage;`

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator`
Expected: PASS (new tests + no regressions).

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/lib.rs
git commit -m "feat(emulator): drive Kitty graphics decode in feed (classic + rich)"
```

---

### Task 7: Host — scaled RGBA blit in `rasterize_frame`

**Files:**
- Modify: `crates/prism-host/src/raster.rs` — add `blit_rgba_scaled`.
- Modify: `crates/prism-host/src/main.rs` — call it in `rasterize_frame` after each pane's guest-screen paint (~line 1246), for ALL panes.
- Test: `crates/prism-host/src/raster.rs` `#[cfg(test)]` for the scaler; host paint verified by the Stage-1 acceptance (Task 8).

**Interfaces:**
- Consumes: `pane.emulator.images()`, `host.font.cell_w/cell_h`, pane `content_x/guest_y/content_w/guest_h`, `scroll`, `screen().scrolled_lines()`.
- Produces: `pub(crate) fn blit_rgba_scaled(buffer, stride, rgba, src_w, src_h, dst_x, dst_y, dst_w, dst_h, clip_x0, clip_y0, clip_x1, clip_y1)` — nearest-neighbor scale + alpha over-blend (reuses the `blit_rgba` blend math).

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-host/src/raster.rs  (in #[cfg(test)] mod)
#[test]
fn scaled_blit_upscales_and_clips() {
    // 1x1 opaque red, scaled into a 2x2 dst on a 4x4 white buffer, clipped to x<3.
    let mut buf = vec![pack_rgb([255, 255, 255]); 16];
    let rgba = [255u8, 0, 0, 255];
    blit_rgba_scaled(&mut buf, 4, &rgba, 1, 1, 1, 1, 2, 2, 0, 0, 3, 4);
    // (1,1),(2,1),(1,2),(2,2) become red; (3,*) clipped out stays white.
    assert_eq!(unpack_rgb(buf[1 * 4 + 1]), [255, 0, 0]);
    assert_eq!(unpack_rgb(buf[2 * 4 + 2]), [255, 0, 0]);
    assert_eq!(unpack_rgb(buf[1 * 4 + 3]), [255, 255, 255]); // clipped column
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-host scaled_blit`
Expected: FAIL — `blit_rgba_scaled` not found.

- [ ] **Step 3: Write minimal implementation**

Add to `crates/prism-host/src/raster.rs`:
```rust
/// Alpha-blit `rgba` (src_w×src_h) scaled to a dst_w×dst_h rect at (dst_x,dst_y),
/// nearest-neighbor, clipped to [clip_x0,clip_x1)×[clip_y0,clip_y1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn blit_rgba_scaled(
    buffer: &mut [u32],
    stride: usize,
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    dst_x: i32,
    dst_y: i32,
    dst_w: usize,
    dst_h: usize,
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    for dy in 0..dst_h {
        let sy = dy * src_h / dst_h;
        let py = dst_y + dy as i32;
        if py < clip_y0 || py >= clip_y1 || py < 0 {
            continue;
        }
        for dx in 0..dst_w {
            let sx = dx * src_w / dst_w;
            let px = dst_x + dx as i32;
            if px < clip_x0 || px >= clip_x1 || px < 0 {
                continue;
            }
            let si = (sy * src_w + sx) * 4;
            if si + 3 >= rgba.len() {
                continue;
            }
            let a = rgba[si + 3];
            if a == 0 {
                continue;
            }
            let (px, py) = (px as usize, py as usize);
            if px >= stride {
                continue;
            }
            let idx = py * stride + px;
            if idx >= buffer.len() {
                continue;
            }
            let dest = unpack_rgb(buffer[idx]);
            let af = a as f32 / 255.0;
            buffer[idx] = pack_rgb([
                (rgba[si] as f32 * af + dest[0] as f32 * (1.0 - af)) as u8,
                (rgba[si + 1] as f32 * af + dest[1] as f32 * (1.0 - af)) as u8,
                (rgba[si + 2] as f32 * af + dest[2] as f32 * (1.0 - af)) as u8,
            ]);
        }
    }
}
```

In `crates/prism-host/src/main.rs`, in `rasterize_frame`, after the guest `rasterize_screen_at_with_theme` call that ends at line ~1246 (and before/around the `if pane.experimental_rich()` overlay block), add — NOT gated on rich:
```rust
        // Inline Kitty-graphics images for this pane (any mode).
        let cw = host.font.cell_w;
        let ch = host.font.cell_h;
        let clip_x0 = content_x as i32;
        let clip_y0 = guest_y as i32;
        let clip_x1 = (content_x + content_w) as i32;
        let clip_y1 = (guest_y + guest_h) as i32;
        let base_abs = pane.emulator.screen().scrolled_lines();
        for img in pane.emulator.images() {
            // Row on screen after accounting for absolute-line anchor and scroll.
            let rel_line = img.anchor_abs_line as i64 - base_abs as i64 + scroll as i64;
            if rel_line < 0 {
                continue; // scrolled above the viewport
            }
            let dst_x = content_x as i32 + img.anchor_col as i32 * cw as i32;
            let dst_y = guest_y as i32 + rel_line as i32 * ch as i32;
            let dst_w = img.cols.max(1) as usize * cw;
            let dst_h = img.rows.max(1) as usize * ch;
            raster::blit_rgba_scaled(
                buffer,
                width as usize,
                &img.rgba,
                img.width as usize,
                img.height as usize,
                dst_x,
                dst_y,
                dst_w,
                dst_h,
                clip_x0,
                clip_y0,
                clip_x1,
                clip_y1,
            );
        }
```
> `scroll` here is the value already computed at `main.rs:1183`. Confirm `raster::blit_rgba_scaled` is `pub(crate)` and reachable (module is `raster`). If `cols`/`rows` from the emulator are 0, `blit` covers one cell; refine the emulator span heuristic (Task 5 note) so the logo occupies a sensible footprint — a good first heuristic in `GraphicsState::store` is `cols = ceil(width / 10)`, `rows = ceil(height / 20)` using nominal 10×20 px cells, clamped to the screen.

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-host scaled_blit && cargo build -p prism-host`
Expected: PASS + clean build.

- [ ] **Step 5: Commit**

```bash
git add crates/prism-host/src/raster.rs crates/prism-host/src/main.rs
git commit -m "feat(host): blit inline Kitty-graphics images (scaled) in all panes"
```

---

### Task 8: Stage-1 end-to-end acceptance (manual)

**Files:** none (manual verification + notes).

- [ ] **Step 1: Build and install the host**

Run: `. "$HOME/.cargo/env" && cargo build -p prism-host --release`

- [ ] **Step 2: Run Claude Code in prism-host with NO rich flag**

Launch the freshly built host running `claude` (however the host normally launches a child; e.g. `./target/release/prism-host claude` or via Prism.app). Trigger the logo/image render.

- [ ] **Step 3: Verify**

Expected: the Claude logo renders as pixels (not a text placeholder), positioned at its cursor cell, within the pane. If it still shows a placeholder, capture the raw child bytes (temporary `eprintln!` of the `_G` control in `GraphicsState::handle`) to confirm transport/format, and branch: `t=f` → Stage 2; chunked (`m=1`) → Task 11.

- [ ] **Step 4: Record findings**

Note in the PR description which transport Claude used and whether Stage 2 / Task 11 are required for the logo specifically (they are required for larger images regardless).

---

## STAGE 2 — `t=f` file transport

### Task 9: Safe bounded file read

**Files:**
- Create: `crates/prism-emulator/src/graphics/file_transport.rs`
- Modify: `crates/prism-emulator/src/graphics/mod.rs` — `pub(crate) mod file_transport;`
- Test: in `file_transport.rs`.

**Interfaces:**
- Produces: `pub(crate) fn read_file_bounded(path: &std::path::Path, max_bytes: u64) -> Result<Vec<u8>, FileError>`; `pub(crate) enum FileError { Open, NotRegular, WrongOwner, TooLarge, Io }`. Opens the file, `fstat`s the OPEN fd (no re-lookup), requires a regular file owned by the current uid and within `max_bytes`, then reads.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/file_transport.rs  (bottom)
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_regular_file_within_cap() {
        let dir = std::env::temp_dir().join(format!("prism-gfx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ok.bin");
        std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
        assert_eq!(read_file_bounded(&p, 1024).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_oversize() {
        let dir = std::env::temp_dir().join(format!("prism-gfx-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.bin");
        std::fs::File::create(&p).unwrap().write_all(&[0u8; 4096]).unwrap();
        assert!(matches!(read_file_bounded(&p, 16), Err(FileError::TooLarge)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_directory() {
        let dir = std::env::temp_dir().join(format!("prism-gfx-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(read_file_bounded(&dir, 1024), Err(FileError::NotRegular) | Err(FileError::Open)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::file_transport`
Expected: FAIL — not found.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/prism-emulator/src/graphics/file_transport.rs  (top)
//! Safe, bounded read of a child-supplied file (Kitty `t=f`). Opens the path,
//! then validates the OPEN fd (no re-lookup) to defeat TOCTOU.

use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileError {
    Open,
    NotRegular,
    WrongOwner,
    TooLarge,
    Io,
}

#[cfg(unix)]
pub(crate) fn read_file_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, FileError> {
    use std::os::unix::fs::MetadataExt;

    let mut file = File::open(path).map_err(|_| FileError::Open)?;
    // fstat the open handle — not the path.
    let meta = file.metadata().map_err(|_| FileError::Io)?;
    if !meta.file_type().is_file() {
        return Err(FileError::NotRegular);
    }
    // Owner must be us. (getuid via libc-free path: compare to our own file's uid.)
    let our_uid = own_uid();
    if meta.uid() != our_uid {
        return Err(FileError::WrongOwner);
    }
    if meta.len() > max_bytes {
        return Err(FileError::TooLarge);
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    // Cap the read too, in case the file grew between fstat and read.
    file.by_ref()
        .take(max_bytes)
        .read_to_end(&mut buf)
        .map_err(|_| FileError::Io)?;
    if buf.len() as u64 > max_bytes {
        return Err(FileError::TooLarge);
    }
    Ok(buf)
}

#[cfg(unix)]
fn own_uid() -> u32 {
    // Derive our uid without adding a libc dep: stat a fd we own (stdin's
    // owner is unreliable) — instead read from the process via /proc is Linux
    // only. Portable approach: create+stat a temp file's uid is our euid.
    use std::os::unix::fs::MetadataExt;
    // A file we just created is owned by our effective uid.
    let tmp = std::env::temp_dir().join(format!(".prism-uid-{}", std::process::id()));
    if let Ok(f) = File::create(&tmp) {
        if let Ok(m) = f.metadata() {
            let uid = m.uid();
            let _ = std::fs::remove_file(&tmp);
            return uid;
        }
    }
    0
}

#[cfg(not(unix))]
pub(crate) fn read_file_bounded(_path: &Path, _max_bytes: u64) -> Result<Vec<u8>, FileError> {
    Err(FileError::Open) // t=f unsupported off-unix in the first cut
}
```
> If a `libc` dependency is already in the workspace lock, prefer `libc::geteuid()` for `own_uid()` and delete the temp-file hack. Check `Cargo.lock` for `libc`; if present, add `libc` to `prism-emulator` deps and use it. (This is the cleaner implementation — the reviewer should prefer it.)

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator graphics::file_transport`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/file_transport.rs crates/prism-emulator/src/graphics/mod.rs
git commit -m "feat(emulator): safe bounded file read for Kitty t=f transport"
```

---

### Task 10: Wire `t=f` into `GraphicsState`

**Files:**
- Modify: `crates/prism-emulator/src/graphics/mod.rs` — handle `Transport::File`.
- Test: in `graphics/mod.rs`.

**Interfaces:**
- Consumes: Task 9 `read_file_bounded`. The payload for `t=f` is a base64-encoded file PATH (per Kitty spec), so: base64-decode payload → UTF-8 path → `read_file_bounded` → `decode_png_bounded`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/mod.rs  (in #[cfg(test)] mod tests)
#[test]
fn transmit_file_transport_reads_and_decodes() {
    use std::io::Write;
    const RED_1X1_PNG: &[u8] = &[
        0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A,0x00,0x00,0x00,0x0D,0x49,0x48,0x44,0x52,
        0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x01,0x08,0x02,0x00,0x00,0x00,0x90,0x77,0x53,
        0xDE,0x00,0x00,0x00,0x0C,0x49,0x44,0x41,0x54,0x08,0xD7,0x63,0xF8,0xCF,0xC0,0x00,
        0x00,0x00,0x03,0x01,0x01,0x00,0x18,0xDD,0x8D,0xB0,0x00,0x00,0x00,0x00,0x49,0x45,
        0x4E,0x44,0xAE,0x42,0x60,0x82,
    ];
    let dir = std::env::temp_dir().join(format!("prism-gfx-tf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("logo.png");
    std::fs::File::create(&p).unwrap().write_all(RED_1X1_PNG).unwrap();
    // Payload for t=f is base64 of the path string.
    let path_b64 = super::base64_encode_for_test(p.to_str().unwrap().as_bytes());

    let mut g = GraphicsState::new();
    let screen = prism_core::Screen::new(80, 24, 0);
    let mut replies = Vec::new();
    let apc = prism_protocol::GraphicsApc {
        control: "a=T,t=f,f=100,i=3".into(),
        payload: path_b64.into_bytes(),
    };
    g.handle(&apc, &screen, &mut replies, 80);
    assert_eq!(g.images().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
```
> Add a tiny `pub(crate) fn base64_encode_for_test(bytes: &[u8]) -> String` in `base64.rs` (test helper) so the test can encode the path without a new dep.

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator transmit_file_transport`
Expected: FAIL — File transport returns early (Task 5 dropped non-Direct).

- [ ] **Step 3: Write minimal implementation**

In `GraphicsState::handle`, replace the `Action::Transmit` arm's early-return guard and add a file branch:
```rust
            Action::Transmit => {
                if cmd.more || cmd.format != PNG_FORMAT {
                    return; // m= handled in Task 11; PNG-only first cut
                }
                match cmd.transport {
                    Transport::Direct => {
                        self.transmit_inline(&cmd, &apc.payload, screen, metrics_cols)
                    }
                    Transport::File => {
                        self.transmit_file(&cmd, &apc.payload, screen, metrics_cols)
                    }
                    Transport::Other => {}
                }
            }
```
Add the method:
```rust
    fn transmit_file(
        &mut self,
        cmd: &command::GraphicsCommand,
        payload_b64: &[u8],
        screen: &Screen,
        metrics_cols: u16,
    ) {
        let Ok(path_bytes) = base64::decode(payload_b64) else { return };
        let Ok(path_str) = String::from_utf8(path_bytes) else { return };
        let Ok(raw) = file_transport::read_file_bounded(
            std::path::Path::new(&path_str),
            MAX_FRAME_BYTES as u64,
        ) else {
            return;
        };
        let max = MaxDims { w: MAX_W, h: MAX_H, bytes: MAX_FRAME_BYTES };
        let Ok(img) = decode_png_bounded(&raw, max) else { return };
        self.store(cmd, img, screen, metrics_cols);
    }
```
Add `use` for `file_transport` at the top of `mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/mod.rs crates/prism-emulator/src/graphics/base64.rs
git commit -m "feat(emulator): render Kitty t=f file-transport images (validated)"
```

---

### Task 11: `m=` chunk reassembly

**Files:**
- Modify: `crates/prism-emulator/src/graphics/mod.rs` — add reassembly scratch + logic.
- Test: in `graphics/mod.rs`.

**Interfaces:**
- Behavior: a `t=d` transmit with `m=1` starts/continues reassembly keyed by image id (id from the FIRST chunk; continuation chunks carry only `m=`). On `m=0` (final chunk), concatenate all base64 payload bytes, then decode as in `transmit_inline`. Enforce `MAX_GRAPHICS_APC_BYTES` on the accumulated base64. One in-flight reassembly per id; a new first-chunk for the same id resets it.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/mod.rs  (in #[cfg(test)] mod tests)
#[test]
fn reassembles_chunked_inline_png() {
    const B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    let (a, b) = B64.split_at(40);
    let mut g = GraphicsState::new();
    let screen = prism_core::Screen::new(80, 24, 0);
    let mut replies = Vec::new();
    // First chunk: full control, m=1.
    g.handle(
        &prism_protocol::GraphicsApc { control: "a=T,t=d,f=100,i=8,m=1".into(), payload: a.as_bytes().to_vec() },
        &screen, &mut replies, 80,
    );
    assert!(g.images().is_empty(), "no image until final chunk");
    // Final chunk: continuation, m=0.
    g.handle(
        &prism_protocol::GraphicsApc { control: "m=0".into(), payload: b.as_bytes().to_vec() },
        &screen, &mut replies, 80,
    );
    assert_eq!(g.images().len(), 1);
    assert_eq!(g.images()[0].id, 8);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator reassembles_chunked`
Expected: FAIL — `m=1` currently returns early (Task 5/10 guard).

- [ ] **Step 3: Write minimal implementation**

Add scratch to `GraphicsState`:
```rust
    /// In-flight chunk reassembly (id -> accumulated base64 + first-chunk cmd).
    pending: Option<Pending>,
```
```rust
#[derive(Debug)]
struct Pending {
    cmd: command::GraphicsCommand,
    base64: Vec<u8>,
}
```
Rewrite the `Action::Transmit` arm to route chunked transmits:
```rust
            Action::Transmit => {
                if cmd.format != PNG_FORMAT && !self.pending.is_some() {
                    // First chunk of a non-PNG image: reject.
                    if !cmd.more { return; }
                }
                if cmd.more || self.pending.is_some() {
                    self.accumulate(&cmd, &apc.payload, screen, metrics_cols);
                } else {
                    match cmd.transport {
                        Transport::Direct => self.transmit_inline(&cmd, &apc.payload, screen, metrics_cols),
                        Transport::File => self.transmit_file(&cmd, &apc.payload, screen, metrics_cols),
                        Transport::Other => {}
                    }
                }
            }
```
Add:
```rust
    fn accumulate(
        &mut self,
        cmd: &command::GraphicsCommand,
        payload_b64: &[u8],
        screen: &Screen,
        metrics_cols: u16,
    ) {
        // Start a new reassembly on a first chunk (carries the real control).
        if self.pending.is_none() {
            if cmd.transport != Transport::Direct || cmd.format != PNG_FORMAT {
                return; // first cut: chunked reassembly for inline PNG only
            }
            self.pending = Some(Pending { cmd: *cmd, base64: Vec::new() });
        }
        let over = {
            let p = self.pending.as_mut().expect("pending set above");
            if p.base64.len() + payload_b64.len() > prism_protocol::MAX_GRAPHICS_APC_BYTES {
                true
            } else {
                p.base64.extend_from_slice(payload_b64);
                false
            }
        };
        if over {
            self.pending = None;
            return;
        }
        if !cmd.more {
            let Pending { cmd, base64 } = self.pending.take().expect("pending");
            self.transmit_inline(&cmd, &base64, screen, metrics_cols);
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/mod.rs
git commit -m "feat(emulator): reassemble m= chunked inline Kitty images"
```

---

## STAGE 3 — Mux parity + reattach

### Task 12: Mux server renders images for its viewer

**Files:**
- Modify: `crates/prism-mux/src/rich.rs` and/or `crates/prism-mux/src/live.rs` — ensure the mux path drives `emulator.feed` (it already does via `process_rich_chunk`) so `emulator.images()` is populated on the server-owned emulator. The windowed host reading a mux-served pane already calls `pane.emulator.images()` (Task 7), so parity is automatic once the mux path constructs an `Emulator` (both `new`/`new_experimental` now carry graphics — verify the classic mux pane at `mux.rs:312` / `live.rs:184` also feeds through a path that calls `feed`).
- Test: `crates/prism-mux/` integration test mirroring the emulator unit test — feed a `_G` transmit and assert `emulator.images()` is populated on the server side.

**Interfaces:**
- Consumes: `Emulator::images()`, `Emulator::feed`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-mux/src/rich.rs  (in #[cfg(test)] mod, mirror an existing rich test)
#[test]
fn mux_server_stores_inline_image() {
    const B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    // Use the plain constructor: graphics must not depend on the experimental path.
    let mut emulator = Emulator::new(80, 24, 10);
    let seq = format!("\x1b_Ga=T,t=d,f=100,i=11;{B64}\x1b\\");
    let _ = emulator.feed(seq.as_bytes());
    assert_eq!(emulator.images().len(), 1);
    assert_eq!(emulator.images()[0].id, 11);
}
```

- [ ] **Step 2: Run test to verify it fails or passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-mux mux_server_stores_inline_image`
Expected: PASS immediately IF the emulator wiring (Task 6) is complete (graphics is emulator-owned). If it fails, the mux pane is constructed in a way that never calls `feed` on the server emulator — trace `process_rich_chunk`/live feed and fix so server output flows through `emulator.feed`.

- [ ] **Step 3: (If needed) route server output through feed** — otherwise no code change; the test documents parity.

- [ ] **Step 4: Confirm no regressions**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-mux`

- [ ] **Step 5: Commit**

```bash
git add crates/prism-mux/src/rich.rs
git commit -m "test(mux): server-owned emulator stores inline Kitty images"
```

---

### Task 13: Detach/reattach restoration

**Files:**
- Modify: `crates/prism-mux/src/live.rs` — on a new viewer attach, the server-owned `Emulator` already holds `images()`; verify the windowed host repaints them on attach (a redraw is requested on attach). Add an invalidation on PTY-generation change (Task 15 shares this).
- Test: integration in `crates/prism-mux/tests/` if an attach harness exists (see `interactive_attach.rs`); else a focused unit test asserting images survive a simulated reattach (state is retained on the same `Emulator`).

- [ ] **Step 1: Write the failing test** (mirror `interactive_attach.rs` structure)

```rust
// crates/prism-mux/tests/interactive_attach.rs  (new test, adapt to the harness)
#[test]
fn image_survives_reattach() {
    // Feed an image, simulate detach + attach of a new viewer, assert the
    // server still exposes the image (no task rerun).
    // (Fill in using the existing attach harness helpers in this file.)
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-mux image_survives_reattach`
Expected: FAIL initially (or compile error until harness wired).

- [ ] **Step 3: Implement** — ensure attach triggers a repaint that reads `images()`; no image state is dropped on detach.

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-mux image_survives_reattach`

- [ ] **Step 5: Commit**

```bash
git add crates/prism-mux/tests/interactive_attach.rs crates/prism-mux/src/live.rs
git commit -m "feat(mux): restore inline images on viewer reattach"
```

---

## STAGE 4 — Cleanup + isolation

### Task 14: Scoped `a=d` delete

**Files:**
- Modify: `crates/prism-emulator/src/graphics/mod.rs` — implement the `Action::Delete` arm.
- Test: in `graphics/mod.rs`.

**Interfaces:**
- Behavior: `a=d` with `d=i,i=<id>` (or bare `i=<id>`) removes only that image; `a=d,d=A` (delete all for THIS emulator) clears this pane's images only — never cross-pane. First cut: support delete-by-id and delete-all-in-this-pane.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/graphics/mod.rs  (in #[cfg(test)] mod tests)
#[test]
fn delete_by_id_removes_only_that_image() {
    const B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    let mut g = GraphicsState::new();
    let screen = prism_core::Screen::new(80, 24, 0);
    let mut r = Vec::new();
    for id in [1u32, 2] {
        g.handle(&prism_protocol::GraphicsApc {
            control: format!("a=T,t=d,f=100,i={id}"), payload: B64.as_bytes().to_vec(),
        }, &screen, &mut r, 80);
    }
    assert_eq!(g.images().len(), 2);
    g.handle(&prism_protocol::GraphicsApc { control: "a=d,d=i,i=1".into(), payload: vec![] }, &screen, &mut r, 80);
    assert_eq!(g.images().len(), 1);
    assert_eq!(g.images()[0].id, 2);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator delete_by_id`
Expected: FAIL — Delete arm is a no-op.

- [ ] **Step 3: Write minimal implementation**

Replace the `Action::Delete` arm:
```rust
            Action::Delete => {
                let all = apc.control.split(',').any(|p| p == "d=A" || p == "d=a");
                if all {
                    self.clear();
                } else if cmd.id != 0 {
                    if let Some(pos) = self.images.iter().position(|p| p.id == cmd.id) {
                        let dropped = self.images.remove(pos);
                        self.retained_bytes = self.retained_bytes.saturating_sub(dropped.rgba.len());
                    }
                }
            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator delete_by_id`

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/graphics/mod.rs
git commit -m "feat(emulator): scoped a=d image deletion (per-pane, by id)"
```

---

### Task 15: Invalidate on clear / alt-switch / resize

**Files:**
- Modify: `crates/prism-emulator/src/lib.rs` — call `self.graphics.clear()` where the screen is cleared (ED `CSI 2 J` full clear / `csi_dispatch`), on alternate-screen enter/leave (DECSET/DECRST `?1049`/`?47`/`?1047`), and in `resize`.
- Test: in `crates/prism-emulator/src/lib.rs`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/prism-emulator/src/lib.rs  (#[cfg(test)])
#[test]
fn resize_invalidates_images() {
    const B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    let mut emulator = Emulator::new(80, 24, 0);
    let _ = emulator.feed(format!("\x1b_Ga=T,t=d,f=100,i=1;{B64}\x1b\\").as_bytes());
    assert_eq!(emulator.images().len(), 1);
    emulator.resize(100, 30);
    assert!(emulator.images().is_empty(), "resize must invalidate images");
}

#[test]
fn full_clear_invalidates_images() {
    const B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    let mut emulator = Emulator::new(80, 24, 0);
    let _ = emulator.feed(format!("\x1b_Ga=T,t=d,f=100,i=1;{B64}\x1b\\").as_bytes());
    let _ = emulator.feed(b"\x1b[2J");
    assert!(emulator.images().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator invalidates_images`
Expected: FAIL — no invalidation yet.

- [ ] **Step 3: Write minimal implementation**

- In `Emulator::resize` (`lib.rs:337`), after resizing the screen, add `self.graphics.clear();`.
- In `ScreenPerformer::csi_dispatch`, the performer cannot see `self.graphics` (it borrows other fields). Add a post-feed reconciliation: track the screen's `content_epoch` before/after `parser.advance` in `feed`, and when a full clear / alt-switch is detected, call `self.graphics.clear()`. Simplest robust rule for the first cut: if `screen.alt_active()` changed across the feed, or the screen reports a full clear (add a `Screen` flag/counter such as `full_clears()` if not present, incremented on ED2), clear graphics. Implement by capturing `let alt_before = self.screen.alt_active();` and a clear-counter before `parser.advance`, then after: `if self.screen.alt_active() != alt_before || self.screen.full_clears() != clears_before { self.graphics.clear(); }`.
  - If `Screen` has no `full_clears()` counter, add one in `prism-core` (increment in the ED-2 handler) — a minimal `pub const fn full_clears(&self) -> u64` accessor and a `u64` field bumped where `CSI 2 J` clears the grid. Wire it in a sub-step.

- [ ] **Step 4: Run test to verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator invalidates_images`

- [ ] **Step 5: Commit**

```bash
git add crates/prism-emulator/src/lib.rs crates/prism-core/src/lib.rs
git commit -m "feat(emulator): invalidate images on resize, full clear, alt-switch"
```

---

### Task 16: Memory-cap regression test + final sweep

**Files:**
- Test: `crates/prism-emulator/src/graphics/mod.rs` — assert the per-generation cap evicts.
- Modify: none expected (cap logic landed in Task 5); this task locks it with a test and runs the full sweep.

- [ ] **Step 1: Write the failing/covering test**

```rust
// crates/prism-emulator/src/graphics/mod.rs  (in #[cfg(test)] mod tests)
#[test]
fn retained_bytes_never_exceed_cap() {
    // Store many small images with distinct ids; retained_bytes stays <= cap.
    const B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAMDAQAY3Y2wAAAAAElFTkSuQmCC";
    let mut g = GraphicsState::new();
    let screen = prism_core::Screen::new(80, 24, 0);
    let mut r = Vec::new();
    for id in 0..50u32 {
        g.handle(&prism_protocol::GraphicsApc {
            control: format!("a=T,t=d,f=100,i={}", id + 1), payload: B64.as_bytes().to_vec(),
        }, &screen, &mut r, 80);
    }
    assert!(g.retained_bytes <= MAX_RETAINED_BYTES);
}
```
(If `retained_bytes` is private, add `#[cfg(test)] pub(crate) fn retained_bytes(&self) -> usize` or make the field `pub(crate)`.)

- [ ] **Step 2: Run test**

Run: `. "$HOME/.cargo/env" && cargo test -p prism-emulator retained_bytes_never_exceed_cap`
Expected: PASS.

- [ ] **Step 3: Full workspace sweep**

Run:
```bash
. "$HOME/.cargo/env"
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: fmt clean, no clippy warnings, all tests pass.

- [ ] **Step 4: Manual acceptance re-run** — repeat Task 8 with Claude Code (both a small `t=d` and a larger `t=f` image), and once through a mux attach.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "test(emulator): lock per-generation raster memory cap; final sweep"
```

---

## Self-Review

**Spec coverage:**
- Root cause / negotiation → Tasks 5, 6 (query `;OK` reply, classic-path intake). ✓
- `t=d` inline + PNG bounds → Tasks 1,3,4,5. ✓
- `t=f` file transport + validation → Tasks 9,10. ✓
- `m=` reassembly → Task 11. ✓
- Host blit + scaling + quiet idle → Task 7 (dirty/redraw via existing drain path; no timer). ✓
- Mux ownership + reattach → Tasks 12,13. ✓
- Cleanup/isolation (`a=d`, invalidation, memory cap) → Tasks 14,15,16. ✓
- Works by default (Global Constraint) → intake in `Emulator` (Task 6), render not gated on rich (Task 7). ✓
- Gates (bounds fail-closed, PTY progress, geometry in-pane, parity) → covered by tests in Tasks 4,5,7,12 + manual Task 8/16.

**Known follow-ups (out of first cut, noted not hidden):**
- `f=24`/`f=32` raw RGBA transmission; `o=z` zlib payloads; `t=t`/`t=s` transports; animation; Unicode/virtual placement (tmux relay). Add only if a real producer needs them.
- Exact `cols`/`rows` footprint heuristic (Task 5 note) may need tuning against the real Claude logo (Task 8 records findings).
- Remote mux-attach terminal client rendering (re-emitting graphics to its own terminal) is explicitly out of scope.

**Type consistency:** `GraphicsApc { control, payload }`, `GraphicsCommand`, `Action`/`Transport`, `DecodedImage`, `MaxDims`, `PlacedImage`, `GraphicsState::{handle, images, clear}`, `blit_rgba_scaled`, `read_file_bounded`/`FileError`, `decode_png_bounded`/`DecodeError` are used identically across tasks. `Emulator::images() -> &[PlacedImage]` matches the host consumer in Task 7 and mux in Task 12.
