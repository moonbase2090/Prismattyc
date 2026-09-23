# Native-window regression tests

These tests drive real desktop windows. Run them with disposable sessions,
configuration, and displays. Test output belongs under `build/`.

Build the workspace binaries before running a fixture:

```bash
cargo build --workspace --bins --locked
```

## Run Linux tests in a container

Build the test image once. The runner copies the selected binaries into a
private container. It does not need provider accounts or credentials.

```bash
tests/native/docker/run.sh build
PRISMATTYC_BINS="$PWD/target/debug" tests/native/docker/run.sh spaces-e2e
PRISMATTYC_BINS="$PWD/target/debug" tests/native/docker/run.sh host-ux-e2e
PRISMATTYC_BINS="$PWD/target/debug" tests/native/docker/run.sh spaces-e2e-wayland
PRISMATTYC_BINS="$PWD/target/debug" tests/native/docker/run.sh walkthrough-caption-e2e
```

The X11 tests use Xvfb and Openbox. The Wayland test uses headless Weston.
Inspect the output images as well as the command status. The host UX test
stores its results in `build/host-ux-e2e/`.

Run individual self-contained fixtures from the repository root:

```bash
PATH="$PWD/target/debug:$PATH" python3 tests/native/spaces-daily-e2e.py
PATH="$PWD/target/debug:$PATH" python3 tests/native/restart-spaces-e2e.py
python3 tests/native/release-update-e2e.py --pmux target/debug/pmux --out build/update-check
```

Check Space rail transparency and opacity hot reload:

```bash
PRISMATTYC_BINS="$PWD/target/debug" tests/native/docker/run.sh rail-transparency-e2e
```

This check inspects alpha in the host framebuffer. It does not verify compositor blur.

### Focus-border raster measurements

The private Xvfb window fixture can measure release-mode raster cost with the
existing border animation enabled and disabled, both idle and while updating a
terminal row:

```bash
PRISMATTYC_TEST_BORDER_BENCH=1 cargo test --release --locked \
  -p prismattyc-host --bin prismattyc-host \
  render_window_tests::real_window_paint_reaches_the_backend -- --exact --nocapture
```

This opt-in mode runs measurements in place of the fixture's correctness checks;
run the same command without the environment variable for correctness. Each
`BORDER_BENCH` JSON record reports the median and p95 raster duration, cells painted,
and submitted rectangle area for 128 frames after 32 warmup frames. Five trials
alternate case order. Rectangle areas may overlap. Compare frozen baseline and
candidate test binaries with the same fixture, fonts, toolchain and configuration,
and keep other builds idle during measurement.

These measurements exclude presentation, input-to-display latency and scheduling
between frames. They do not establish native macOS, Windows or Wayland performance.
The tile tests separately bound the macOS copy footprint; seven-pixel edge damage
still copies complete intersecting 512-by-128 tiles.

## Run macOS tests

Use a Mac with a logged-in desktop session. Build the same workspace revision
on that Mac. These fixtures do not replace the installed application.

```bash
python3 tests/native/macos-alpha-e2e.py --bins target/debug --out build/macos-alpha
python3 tests/native/macos-restart-e2e.py --bins target/debug --out build/macos-restart
```

Native framebuffer captures and desktop screenshots prove different paths.
Read each fixture's output to see which path it checked. See [test requirements](../../docs/testing-policy.md)
for release validation and [accessibility](../../docs/accessibility.md) for AT-SPI checks.
