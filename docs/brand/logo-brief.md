# Prismattyc — logo / icon design brief

**Status:** v2 assets landed — mark **“Continuous beam” (9d)**.  
**Product:** Prismattyc / `prismattyc-host`  
**Tagline:** Classic terminal. Modern surface.  
**Assets:** [`assets/brand/`](../../assets/brand/) (SVG masters + PNG set; see that README).

Reconstituted from the in-session brief (refraction/facets, host BG `#121214`,
app-icon deliverables) plus current windowed chrome constants in
`crates/prismattyc-host/src/raster.rs`. v1 mark was **“Sheared spectrum.”** v2 is
one white beam, split into the seven-hue spectrum and recombined.

---

## Product

| | |
|--|--|
| **Name** | Prismattyc |
| **Binary** | `prismattyc-host` (windowed OS host) |
| **Tagline** | **Classic terminal. Modern surface.** |
| **One-liner** | A real VT-class terminal in its own window — with room for a modern, opt-in rich surface later — without a browser engine. |

---

## Job of the mark

The logo/icon must read as:

1. **Terminal / developer tool** (dock, launcher, task switcher) — not a consumer app, not a crypto token, not a photo editor.
2. **“Prism” literally** — light split into facets / spectrum, without looking like a rainbow sticker.
3. **Classic + modern** — severe grid/monospace discipline *and* a clean geometric mark that could sit next to Kitty/Ghostty without looking like them.

Primary use: **OS app icon** for `prismattyc-host`. Secondary: README, docs, about dialog, optional window/title decoration.

---

## Concept

**Working concept: “Refracted grid” / faceted prism**

- A **simple prism silhouette** (triangular prism or diamond facet stack), **or** a single bold **P** built from **planar facets**.
- Light enters as a **neutral beam** (classic / mono) and exits as **tight spectral facets** (modern surface) — **2–4 hard edges**, not a full rainbow gradient wash.
- Optional secondary read: faint **cell-grid** or **cursor block** folded into a facet so it still feels like a terminal at 16×16.

Metaphor lock:

| Layer | Meaning |
|-------|---------|
| Dark field | Classic grid, `#121214` void |
| Facets | Structured modern surface (not chrome fluff) |
| Controlled spectrum | Capability / rich path — opt-in, not the whole identity |
| Hard geometry | Precision, VT discipline, no skeuomorphic glass blobs |

---

## Visual system

### Palette (bind to product chrome)

| Role | Hex | Notes |
|------|-----|--------|
| **App / mark background** | **`#121214`** | Host default cell BG (`DEFAULT_BG`) |
| Chrome / elevated panel | `#1B1E26` | Host chrome BG (`CHROME_BG`) |
| Primary ink / light facet | `#D0D0D0` | Default FG (`DEFAULT_FG`) |
| Focus / structure accent | `#62A8FF` | Focus border (`FOCUS_BORDER`) |
| Activity / secondary accent | `#FFB454` | Unseen-output badge (`UNSEEN_BADGE`); use sparingly |
| Border / edge | `#454A57` | Pane border (`PANE_BORDER`) |

Spectrum accents (facet edges only — not full-bleed):

- Cool cyan → blue → soft violet, **desaturated**, on dark
- Avoid neon “gamer RGB” and candy gradients

### Typography (wordmark, if any)

- **Name:** `Prism` — geometric sans or slightly technical mono-influenced sans
- **Not** a full terminal font for the wordmark at app-icon sizes
- Tagline only in marketing: *Classic terminal. Modern surface.*
- Icon-only must work **without** the wordmark

### Form language

- **Hard edges**, few vertices; 1–3 major shapes
- Prefer **flat / soft-flat** over photoreal glass
- Subtle depth: 1 light direction max; no heavy drop shadows
- Readable as a **silhouette** in monochrome

---

## Composition rules

| Rule | Spec |
|------|------|
| Safe area | ~10–12% padding from icon edge (Linux/desktop + future macOS-style masks) |
| Complexity at 16×16 | Still a recognizable triangle/facet or block-P; no fine grid |
| Background | Prefer **solid `#121214`** (or transparent on dark UI only) |
| Corner radius | Follow platform mask; design **as if square**; don’t hard-code squircle |
| Motion | Optional idle: **none** (aligns product: no continuous idle animation) |

---

## Deliverables

### App icon set

| Asset | Size / notes |
|-------|----------------|
| Master vector | SVG (path-based; no embedded bitmap) |
| Raster masters | 1024×1024 PNG (sRGB, no heavy compression artifacts) |
| Linux / freedesktop | 16, 24, 32, 48, 64, 128, 256, 512 PNG |
| Optional HiDPI | 2× variants if you skip pure vector install |
| Monochrome | 1-bit / single-color version for status menus / symbolic icons |

### Brand lockups

| Asset | Use |
|-------|-----|
| Icon only | Dock / window / `.desktop` |
| Icon + wordmark (horizontal) | README header, docs |
| Icon + wordmark (stacked) | Splash / about (if ever) |
| Tagline lockup | Landing / charter graphics only |

### Export / repo notes

- SVG: closed paths, no unexplained strokes that vanish at small size
- PNG: transparent where needed; default presentation on `#121214`
- **Landed:** [`assets/brand/`](../../assets/brand/) — `prismattyc-icon.svg`, tile SVG (dark squircle, Linux desktop), mono SVG, freedesktop PNGs + 1024 tile, macOS SVG at [`macos/prismattyc.svg`](../../assets/brand/macos/prismattyc.svg)

---

## Do / don’t

**Do**

- Dark, precise, “tool-first”
- Facets that read as **structure**, not decoration
- Cool blue as the main accent; amber only as a wink (activity)
- Something that could live next to a monochrome VT aesthetic

**Don’t**

- Browser chrome, play buttons, or “AI orb” clichés
- Full-spectrum rainbow wash or prism-as-disco-ball
- Literal CRT + scanlines as the whole mark
- Crowded multi-pane diagrams inside the icon
- Soft pastel light-mode primary (product is dark-first)
- Trademark-adjacent clones of Kitty/Ghostty/iTerm marks

---

## Success criteria

1. At **32×32**, someone says “terminal / dev tool,” not “photo app.”
2. At **16×16**, the shape still holds (triangle / facet / block).
3. Name **Prism** feels motivated by the mark without spelling it out.
4. Sits on **`#121214`** next to real `prismattyc-host` chrome without palette clash.
5. Monochrome version still works.

---

## References (product, not visual clones)

- Host chrome: `crates/prismattyc-host/src/raster.rs` (`DEFAULT_BG`, `FOCUS_BORDER`, `UNSEEN_BADGE`, …)
- Charter / PRD tagline: *Classic terminal. Modern surface.*
- Positioning: classic fidelity first; modern surface opt-in; **no browser engine**
- Windowed host: [ADR-0006](../adr/0006-windowed-host.md)

---

## Out of scope (this brief)

- ~~Final illustration files~~ → **v1 in `assets/brand/`** (further polish optional)
- `.desktop` install packaging / desktop-file icon install
- Animated splash / tray idle
- Light-mode brand system (optional later)
