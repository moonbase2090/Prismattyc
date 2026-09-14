# Prismattyc documentation

**Classic terminal. Modern surface.**

This directory is the living design and process surface for Prismattyc. Root
[README.md](../README.md) is the entry point; the charter and starting points
live at the repo root as well for easy discovery.

| Document | Purpose | Status |
|----------|---------|--------|
| [../Prismattyc-Charter.md](../Prismattyc-Charter.md) | Vision, principles, goals, non-goals, tenets | Accepted (v0) |
| [../Prismattyc-Starting-Points.md](../Prismattyc-Starting-Points.md) | First development slices | Accepted (v0) |
| [roadmap.md](roadmap.md) | Ordered work + current status | Living |
| [architecture.md](architecture.md) | Major components and data flow | Draft sketch |
| [hybrid-rendering.md](hybrid-rendering.md) | Cell grid + rich layer coexistence (freeze) | v0 freeze |
| [capability-protocol.md](capability-protocol.md) | Opt-in feature discovery (first cut) | Design cut on main |
| [spike-baseline-v0.md](spike-baseline-v0.md) | Phase 0A exit fixture set / allowlist | Published |
| [phase-0b-spike.md](phase-0b-spike.md) | Experimental 0B spike contract | Closed (experimental) |
| [fidelity-matrix-v1.md](fidelity-matrix-v1.md) | Supported classic claim (F1–F19) | Published (`prismattyc-classic/0.1.1`) |
| [workspace.md](workspace.md) | Cargo layout and repo conventions | On main (skeleton) |
| [decisions-v0.md](decisions-v0.md) | MVP architecture freeze (Phase 0); D9 multi-core/GPU; D10 host input | On main |
| [adr/0001-host-selection-clipboard.md](adr/0001-host-selection-clipboard.md) | Host selection, clipboard, key ownership (clean-room) | Accepted |
| [adr/0002-host-mouse-policy.md](adr/0002-host-mouse-policy.md) | Host mouse: selection only (policy freeze) | Accepted (partly superseded by 0003) |
| [adr/0003-hybrid-mouse.md](adr/0003-hybrid-mouse.md) | Hybrid mouse: app SGR + Shift host select | Accepted |
| [adr/0004-wide-unicode.md](adr/0004-wide-unicode.md) | Wide Unicode display width (first slice) | Accepted |
| [adr/0005-kitty-keyboard.md](adr/0005-kitty-keyboard.md) | Kitty CSI-u progressive keyboard | Accepted |
| [adr/0006-windowed-host.md](adr/0006-windowed-host.md) | Windowed OS host (`prismattyc-host`) | Accepted |
| [adr/0007-phase2-mux-domain.md](adr/0007-phase2-mux-domain.md) | Mux domain model | Accepted |
| [adr/0011-long-lived-mux-server.md](adr/0011-long-lived-mux-server.md) | Long-lived local mux server and attach lifecycle | Accepted |
| [a6-evidence.md](a6-evidence.md) | A-6 classic/mux operator evidence pack | Published (A-6 PASS deferred) |
| [13e-windowed-dogfood.md](13e-windowed-dogfood.md) | Windowed host + 2B dogfood checklist | Ready after |
| [phase2b-detach-proof.md](phase2b-detach-proof.md) | deterministic detach/reattach lifetime proof | Landed (on main) |
| [bug-log-0.1.x.md](bug-log-0.1.x.md) | Critical re-eval findings; work queue | Living |
| [mux-cli.md](mux-cli.md) | `pmux` command reference | Living |
| [prismattyc/design-plan.md](prismattyc/design-plan.md) | Fold Switchboard into Prismattyc (`pmux` naming) | Draft |
| [hung-session-recovery.md](hung-session-recovery.md) | Diagnose and recover a frozen attach, session, or mux server | Living |
| [testing-policy.md](testing-policy.md) | Local Actions merge-gate policy; present backends (PT-290) | Living |
| [macos.md](macos.md) | macOS port inventory; not a support claim | Living |
| [gpu-spike.md](gpu-spike.md) | wgpu GPU present/raster spike (D9) | Spike complete |
| [a11y-spike.md](a11y-spike.md) | VoiceOver / Orca host probe (PT-34) | Spike complete |
| [adr/0016-host-accessibility.md](adr/0016-host-accessibility.md) | Host accessibility: AccessKit tree + announce | Accepted |
| [ssh-mux-attach-spike.md](ssh-mux-attach-spike.md) | SSH from macOS to Linux mux attach (no Wayland) | First slice: TTY recipe + display guard |
| [click-hyperlinks-spike.md](click-hyperlinks-spike.md) | click printed `http(s)` URLs in prismattyc-host | First slice: Ctrl/Cmd+click; OSC 8 later |
| [kitty-graphics-rich-experience-spike.md](kitty-graphics-rich-experience-spike.md) | `terminal-browser` Kitty/Herdr research and bounded Runbook artifact-preview recommendation | Spike complete |
| [kitty-graphics-claude-code-not-triggering.md](kitty-graphics-claude-code-not-triggering.md) | Claude Code detector (`TERM.includes("kitty")`) + Prism `prism-kitty` / box-drawing wrap | Resolved |
| [config.md](config.md) | Windowed-host config file + hot reload (notify) | Living |
| [testing-ux.md](testing-ux.md) | Host UX test pyramid: unit, nested PTY, human dogfood | Living |
| [rich-robustness.md](rich-robustness.md) | composition matrix (clip/scroll/alt/teardown) | Living |
| [rich-client.md](rich-client.md) | Public 0.1-0.3 rich-client authoring, fallback, input, semantics, and status | Living |
| [rich-tui-next-phase.md](rich-tui-next-phase.md) | Proposed reusable rich-TUI surface plan | Proposal |
| [PRD.md](PRD.md) | Product requirements (problem, stories, impl/test, OOS) | v0.5 |
| [agents.md](agents.md) | How to work in this repo (mux identity, mail, tests) | Living |
| [termwright.md](termwright.md) | E2E TUI via Termwright | Living |
| [mux-research.md](mux-research.md) | Phase 2 mux competitive notes (tmux/Herdr/…) | Living |
| [brand/logo-brief.md](brand/logo-brief.md) | Prismattyc logo/icon design brief | v2 Continuous beam |
| [../assets/brand/](../assets/brand/) | Brand mark assets (SVG + PNG) | v2 landed |
| [../e2e/README.md](../e2e/README.md) | Termwright scenarios + runner | Living |

## Reading order (new contributors)

1. [Prismattyc-Charter.md](../Prismattyc-Charter.md) — what we are and are not building
2. [roadmap.md](roadmap.md) — what to do first
3. [architecture.md](architecture.md) — how the pieces fit
4. [hybrid-rendering.md](hybrid-rendering.md) and [capability-protocol.md](capability-protocol.md) — the two highest-leverage design surfaces
5. [workspace.md](workspace.md) — where code will live

## Documentation rules

- **Charter wins** on product intent. Design docs must not contradict it; if they must, amend the charter first.
- Mark open questions explicitly. Prefer "undecided" over invented certainty.
- Design sketches are **data for discussion**, not binding ADRs until promoted.
- When a decision hardens, record it here (and later under `docs/adr/` if the set grows).
- Out-of-repo notes are not a substitute for repo docs. Durable project truth lives in git.
