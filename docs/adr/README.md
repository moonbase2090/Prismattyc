# Protocol and input contracts

These records explain implementation decisions and compatibility rules.
Where a record is superseded, use its replacement for current behavior.
User-facing controls are described in the [configuration reference](../config.md).

## Index

| ADR | Title | Status |
|-----|--------|--------|
| [0001](0001-host-selection-clipboard.md) | Host selection, clipboard, and key ownership | Accepted |
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
