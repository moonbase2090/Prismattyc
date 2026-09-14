# Prismattyc

**Classic terminal. Modern surface.**

Prismattyc supersedes the Prism name. Same product, current name.

A native emulator and multiplexer that keeps perfect compatibility with the existing terminal world while giving applications a first-class markup, styling, animation, and canvas layer when they opt in.

---

## Project Charter

### Vision

Evolve the terminal into a surface that remains the most reliable and efficient place for text-based work, yet is capable of hosting richer, more expressive interfaces when applications deliberately opt in — without ever requiring a browser or becoming an IDE.

### Spaces positioning amendment

**Spaces positioning amendment — Brandan, 2026-09-10:** Market Spaces as
an agent-team workspace. This amends the AI-first positioning exclusion
below for Spaces. Agent CLIs reason and choose work. Prismattyc presents
seats and transports mail; it is not an agent orchestration runtime.
See the [Spaces decision and rollback baseline](docs/design/spaces-agent-centric-prd.md#positioning-decision-and-rollback-baseline).

### Core Principles

1. **Classic fidelity first**  
   Every existing program that works in a modern terminal must continue to work correctly and performantly. No regressions in escape sequences, mouse support, alternate screen, scrollback, or keyboard protocols.

2. **Opt-in richness**  
   Markup, styling, animation, and canvas capabilities are strictly opt-in. Applications discover support via capability queries and degrade gracefully. The default path remains pure VT/xterm behavior.

3. **Native binary**  
   Distributed and installed as a normal host binary. No Electron, no required browser runtime, no web server for normal use.

4. **Hybrid rendering model**  
   A traditional cell grid + scrollback coexists cleanly with an optional retained/markup/canvas layer. The layers are designed so they do not fight over cursor, selection, or input.

5. **Multiplexing as a first-class citizen**  
   Sessions, windows, panes, layouts, detach/reattach, and sharing are core features.

6. **Declarative and composable for the rich layer**  
   The modern surface favors clear markup + styling (and canvas primitives) so rich applications stay maintainable and themeable.

7. **Performance and correctness over feature count**  
   Latency, memory use, and correctness of the classic path take priority.

### Primary Goals

- Full, high-quality terminal emulation + multiplexer in one binary
- Clean, versioned protocol extension for markup, styling, animation, and canvas
- Excellent developer experience for both classic and rich applications
- Cross-platform with native feel (Linux primary)

### Explicit Non-Goals

- Becoming a general-purpose IDE or code editor
- Requiring or embedding a full web browser engine
- Forcing every application onto the rich surface
- Competing with closed or AI-first terminal products

### Technical Tenets (initial)

- Prefer Rust for the core
- Reuse proven parsers and PTY handling
- Design the rich protocol early and keep it queryable
- Keep the classic cell-grid path as lean and correct as possible
