# Host chrome behavior preview

Open `index.html` directly in a browser, or serve this directory:

```sh
python3 -m http.server 8838 --bind 127.0.0.1 --directory demo/chrome-effects
```

Then open <http://127.0.0.1:8838/>. No packages or network assets are required.

Compare quiet signals and timed accents. Switch focus and Spaces, ring bells,
try a burst, change reduced motion, and hide the simulated window. The counters
show preview state only. They are not native performance evidence. The preview
uses event handlers and one next-cleanup timeout; no animation frame loop runs.

The [design contract](../../docs/design/chrome-effects.md) describes the proposed
native lifecycle and acceptance criteria. This is an original design artifact;
it does not modify the running host or its configuration.
