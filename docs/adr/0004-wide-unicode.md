# ADR-0004: Wide Unicode (display width) — classic first slice

- **Status:** Accepted
- **Date:** 2026-08-01
- **Charter:** classic path first; no silent layout corruption under common CJK / emoji-adjacent input
- **Related:** [fidelity-matrix-v1.md](../fidelity-matrix-v1.md) (was exclusion) terminfo residual note
- **Clean-room:** policy and Prismattyc code; display width via Unicode East Asian Width (UAX #11) through the
  `unicode-width` crate — not vendored terminal emulator source

## Context

Prismattyc’s classic grid was a **single-column model**: every `char` occupied one cell and advanced the
cursor by one. East Asian fullwidth and other double-width code points (and zero-width marks) then
**desync** the logical cursor from what the outer host paints, breaking layout for common
CJK / fullwidth workloads.

The fidelity matrix listed wide Unicode as an intentional exclusion. Dogfood and residual
`xterm-256color` / `prismattyc-256color` terminfo still surface the gap whenever apps print wide text.

## Decision

### D-W1 — Claim display width for BMP / common wide (first slice)

| Class | Display columns | Behavior |
|-------|-----------------|----------|
| Narrow (width 1) | 1 | Unchanged: store glyph, advance 1 |
| Wide (width 2) | 2 | Store glyph in lead cell; **continuation** cell in the next column; advance 2 |
| Zero-width (width 0) | 0 | **Attach** to previous base cell as combining marks (up to `MAX_COMBINING_MARKS`); do not advance cursor |
| Control / non-print handled elsewhere | — | Existing C0 / sanitize paths unchanged |

**Ambiguous-width** characters follow `unicode-width` defaults (typically narrow) for a stable,
locale-independent classic claim.

### D-W2 — Continuation cell model

A double-width glyph occupies two adjacent cells in the same row:

1. **Lead:** `character = glyph`, `wide_cont = false`
2. **Continuation:** `wide_cont = true` (glyph ignored for paint/extract)

Invariants:

- Continuation never appears in column 0 without a lead (heal/overwrite clears pairs).
- Overwriting either half of a pair clears **both** halves before writing the new glyph.
- If a width-2 glyph does not fit on the current line (`column + 2 > columns`), **wrap**
  (CR+LF / delayed-wrap path) then place — same spirit as xterm-class wide placement.

### D-W3 — Paint and extract

- **ANSI paint:** emit the lead glyph once; **do not** emit a second printable for the
  continuation (outer host advances two columns for a wide glyph).
- **Plain extract / OSC 52 / history text:** include the lead character only; skip
  continuation cells so copy is not doubled.
- **Selection chrome:** both cells may inverse; extract still one code point.

### D-W4 — Combining marks (slice 2)

Zero-width scalars that follow a base glyph are **appended** to that cell's
combining list (capped). Paint and extract emit base then marks. Cursor does
not advance. Marks with no prior base on the line are dropped.

### D-W5 — ZWJ / extended emoji clusters (slice 3)

Streaming attachment (no full UAX #29 grapheme engine):

| Rule | Behavior |
|------|----------|
| **ZWJ join** | If the previous base cell's trailing list **ends with U+200D**, the next non-control scalar is **appended** to that cell and the cursor does **not** advance |
| **Emoji modifiers** | U+1F3FB..=U+1F3FF attach like marks even when `unicode-width` reports width 2 |
| **Regional indicator pairs** | Two consecutive RIs (U+1F1E6..=U+1F1FF) form **one** width-2 cell (flag); second RI appends and adds a continuation |
| **Capacity** | Trailing scalars capped at `MAX_COMBINING_MARKS` (12); excess dropped |
| **Display width** | Cluster width = width of the **first base** (typically 2 for emoji); paint/extract emit the full scalar sequence once |

Not a full extended grapheme cluster claim: no tag sequences, no pure UAX #29 segmentation crate.

### D-W6 — Explicit non-goals

- Full UAX #29 grapheme segmentation / `unicode-segmentation` product claim
- Bidirectional reordering
- Soft wrap reflow of existing lines on resize (still matrix exclusion)
- Emoji presentation selectors beyond zero-width attach already covered by width-0 path

### D-W7 — Dependency

Display width is computed with **`unicode-width`** (Unicode East Asian Width). Version is pinned
in the workspace lockfile. Replacing the crate requires an ADR update only if behavior of
claimed width classes changes. Cluster **join rules** are Prismattyc-owned (not vendor terminal sources).

## Consequences

**Positive**

- CJK and fullwidth punctuation no longer corrupt column geometry for the common case.
- Combining accents (e.g. `e` + U+0301) round-trip in extract and paint.
- Common ZWJ emoji (family, profession) and flag pairs stay one cell for cursor/select/copy.

**Negative**

- More edge cases in ICH/DCH/insert/delete and multi-cell selection until follow-ups harden them.
- Pathological long sequences truncate at the trailing-scalar cap.
- Not every UAX #29 edge case matches a browser/OS grapheme break.

**Follow-ups**

- Human smoke: family emoji / flags under Prismattyc
- Optional: tag sequences / further emoji presentation edge cases

## Mapping to code

| Behavior | Location |
|----------|----------|
| Width class | `prismattyc_core::char_display_width` |
| Cluster helpers | `is_zwj` / `is_emoji_modifier` / `is_regional_indicator` |
| Storage + put | `Cell::wide_cont`, `Cell::combining_*`, `Screen::put_char` |
| ZWJ / skin / RI join | `attach_cluster_scalar`, `try_extend_regional_indicator_pair` |
| Pair clear on overwrite | `Screen` helpers used by `put_char` |
| ICH/DCH/ECH pair expand + heal | `insert_chars` / `delete_chars` / `erase_chars` + `heal_wide_pairs_in_row` |
| Paint skip cont + emit marks | `prismattyc-render` ANSI / plain paths |
| Extract | `write_grapheme_into` / `extract_text*` |

## References

- Unicode Standard Annex #11 (East Asian Width)
- Fidelity matrix wide Unicode exclusion (superseded for this claim slice)
