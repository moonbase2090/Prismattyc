# Claude Code Kitty-graphics trigger in Prism

**Status:** Resolved 2026-08-20 (dogfood: crab + composer rules match Ghostty).

## Symptom (original)

Claude Code's startup chrome in Prism looked gapped / dashed next to Ghostty:
blocky crab, dashed composer rules. Ghostty showed a pixel crab and solid
hairline rules.

## What was actually two problems

1. **Kitty graphics never fired.** Claude Code v2.1.238 classifies the
   terminal with static env (no runtime `a=q` until that passes). Detector
   order extracted from the binary:

   ```js
   if (process.env.TERM === "xterm-ghostty") return "ghostty";
   if (process.env.TERM?.includes("kitty")) return "kitty"; // before TERM_PROGRAM
   if (process.env.TERM_PROGRAM) return process.env.TERM_PROGRAM; // "prism"
   // ...
   if (process.env.KITTY_WINDOW_ID) return "kitty"; // never reached
   ```

   Prism used `TERM=prism-direct` + `TERM_PROGRAM=prism`, so `KITTY_WINDOW_ID`
   was dead. Same class as [anthropics/claude-code#27868](https://github.com/anthropics/claude-code/issues/27868).
   The logo then used Unicode half-blocks (`U+2580`..).

2. **Box-drawing `─` (U+2500) came from the font.** Even after graphics
   worked, composer rules looked dashed because JetBrains Mono's `─` does
   not span the cell. Ghostty draws `U+2500..=U+257F` as sprites
   (`src/font/sprite/draw/box.zig`).

## What shipped

| Change | Why |
|--------|-----|
| `TERM=prism-kitty` (alias `prism-direct`) | Hits `TERM.includes("kitty")`. Caps stay `use=xterm-direct` plus honest extras (`fullkbd`, `Tc`, `setrgbf`/`setrgbb`, paste, focus). Not `use=xterm-kitty`. |
| `KITTY_WINDOW_ID=1` | Secondary signal for tools that check the variable; not sufficient for Claude Code. |
| PTY `ws_xpixel`/`ws_ypixel` = `cols*cell_w` × `rows*cell_h` | Ghostty `Exec.zig` / `pty.zig`. Zero pixels made Claude emit a postage-stamp PNG. Mux-server spawn uses nominal 10×20 (no viewer font); attach `Resize` then sends the outer TTY cell size. |
| XTWINOPS `CSI 14/16/18 t` | Ghostty `size_report.zig`. |
| Procedural box-drawing in the host rasterizer | Solid composer rules; tiles across cells. |
| Block elements `U+2580..=U+259F` (already on main) | Half-block fallback without seams. |

`TERM_PROGRAM` stays `prism`. Name must be lowercase (`Prism-Kitty` would not match).

## Isolation

```bash
# CSI-u push means Claude classified us as kitty-capable
python3 -c '...'  # TERM=prism-kitty → CSI >1u; TERM=prism-direct + KITTY_WINDOW_ID → not
```

Dogfood: restart `prismattyc-host` (not Super+N) so children spawn with the new
`TERM` and pixel winsize. Mux-server children spawn with nominal cell
pixels (10×20) so ioctl is not `0×0`; `prismattyc-mux attach` then reports the
outer `TIOCGWINSZ` cell size on `Resize`.
