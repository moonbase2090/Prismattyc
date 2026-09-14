# Architecture Decision Records (Prismattyc)

One decision per file. Prefer **append / supersede** over silent rewrites of accepted ADRs.

## Process

For host UX and other “solved in other terminals” areas:

1. Study **behavior and public specs** (not paste foreign source).
2. Freeze policy in an ADR in **our** words.
3. Implement **original** Prismattyc code from the ADR.
4. No vendoring of other terminal emulator trees; no GPL implementation without an
   explicit product decision.

See ADR-0001 for the worked example.

## Index

| ADR | Title | Status |
|-----|--------|--------|
| [0001](0001-host-selection-clipboard.md) | Host selection, clipboard, and key ownership | Accepted |
| [0002](0002-host-mouse-policy.md) | Host mouse policy (selection only; no app mouse claim) | Accepted (superseded in part by 0003) |
| [0003](0003-hybrid-mouse.md) | Hybrid mouse (app report + Shift host select) | Accepted |
| [0004](0004-wide-unicode.md) | Wide Unicode display width (first slice) | Accepted |
| [0005](0005-kitty-keyboard.md) | Kitty CSI-u progressive keyboard protocol | Accepted |
| [0006](0006-windowed-host.md) | Windowed OS host (`prismattyc-host`) | Accepted |
| [0007](0007-phase2-mux-domain.md) | Mux domain and geometry ownership | Accepted |
| [0008](0008-control-plane-v0.md) | Local mux control plane v0 | Accepted |
| [0009](0009-controller-leases.md) | Connection-bound pane controller leases | Accepted |
| [0010](0010-windowed-mux-chrome.md) | Windowed mux chrome and direct pane keys | Accepted (tab OOS superseded by 0012) |
| [0011](0011-long-lived-mux-server.md) | Long-lived local mux server and attach lifecycle | Accepted |
| [0012](0012-host-tabs.md) | Host tabs (window-as-tab chrome) | Accepted |
| [0013](0013-rich-surface-v1.md) | Rich surface scope v1 | Superseded for protocol 0.3+ by 0014 |
| [0014](0014-rich-surface-v2-fabric.md) | Rich surface v2 and Runbook boundary | Accepted |
| [0015](0015-user-keybindings.md) | User keybindings for host actions (`[keys]`) | Accepted |
| [0016](0016-host-accessibility.md) | Host accessibility (OS tree + announce) | Accepted |
| [0017](0017-windows-surface.md) | Windows surface (control transport and cfg gates) | Proposed |
