# Native legend proof

Tested revision `47754266530581ba8a69debda94c28dbf0c186e1` from PR #457.
The source checkout stayed clean.

Eight native-window cases passed on Linux aarch64 with Rust 1.99.1, Xvfb,
Openbox, winit, and softbuffer. Each case holds Control and Shift through
X11 input, waits two seconds, captures the actual window with ffmpeg,
and compares every RGB pixel with the buffer handed to the presenter.
The driver also checks that the legend clears after modifier release.

Both built-in dark and light themes passed at scale factors 1 and 2.
The tab strip measures 44 physical pixels at 1x and 88 at 2x.
Wide windows measure 1280x700 and 2560x1400 physical pixels.
Narrow windows measure 530x420 and 1060x840 physical pixels.
Visual inspection confirms that each narrow legend keeps the complete
`split>` action and omits the following `splitv` action, including both
keycaps and its caption. The wide legends include that action.

`result.json` records the cases and screenshot hashes.
`binary-sha256.txt` identifies all four binaries from the successful build.
The screenshots contain only a private fixture session with static text.
This run covers the native X11 window path. It does not establish macOS,
Wayland, or a full workspace gate pass.

## Repeat the capture

Use Linux with Rust 1.90 or newer, a C compiler, Fontconfig and X11 development
libraries, Xvfb, Openbox, xdotool, xdpyinfo, ffmpeg, and DejaVu fonts.
Run in a private container where X11 display 99 is available.

```sh
git clone https://github.com/moonbase2090/Prismattyc source
git -C source checkout 47754266530581ba8a69debda94c28dbf0c186e1
cargo build --locked --manifest-path source/Cargo.toml --target-dir target -p prismattyc-host -p prismattyc-mux --bins
python3 capture-legend.py
```

The expected output has eight `PASS` lines and writes `captures/result.json`.
Inspect all eight screenshots before using them as review evidence.
