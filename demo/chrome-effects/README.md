# Pane-local bell preview

Serve with `python3 -m http.server 8838 --bind 127.0.0.1 --directory demo/chrome-effects`
and open <http://127.0.0.1:8838/>, or open `index.html` directly.

This replaces the earlier broad design comparison after PT-308 was narrowed.
Ring either pane, send a burst, switch focus/Space, or hide the simulated window.
The optional bell uses a fixed 120 ms perimeter with one cleanup deadline and no
animation loop. Existing static focus and background cues remain illustrative.

No external packages/assets are required. This is an original interaction preview,
not a native visual oracle or performance result. See the
[implementation contract](../../docs/design/chrome-effects.md).
