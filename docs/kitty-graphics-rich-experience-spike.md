# Use a bounded raster asset plane for Prism's rich experience

- **Status:** Research complete; design recommendation, not a protocol freeze
- **Date:** August 19, 2026
- **Reviewed source:** `terminal-browser` v0.5.8 at
  `9265a06fb875e5ed810359bbf007bd46d1156a3a`
- **Prism baseline:** `ee57e08ecad772d5d3c7cae52a1d43ae7aec76d6`

## Overview

This analysis examines how `terminal-browser` moves Chromium pixels through
Kitty graphics transports. It identifies the parts that you can adapt to
Prism's rich experience without weakening Prism's semantic model, classic
fallback, isolation boundaries, or quiet-idle behavior.

The analysis follows the complete path from Chromium's offscreen frame to the
terminal. It covers capability detection, pixel conversion, damage tracking,
transport selection, tmux relay, buffer ownership, acknowledgment, cleanup,
and idle behavior. It also compares the direct Kitty path with
`terminal-browser`'s Herdr side channel.

You should use a narrow, experimental raster asset plane for one practical
task: **Runbook artifact preview**. A selected failure can show one bounded
screenshot, visual diff, chart, or flame graph in the existing detail region.
Keep the task, diagnostic, action, artifact metadata, and alternative text in
the semantic rich tree. In classic mode, print a useful description and a safe
artifact reference.

Do not convert the Runbook workspace into a framebuffer. Do not send large
pixel payloads through Prism's 4,096-byte application program command (APC)
control messages. Do not treat raw Kitty escape sequences as Prism's rich
protocol. `terminal-browser` provides useful patterns for transport probing,
fallback, buffer ownership, and replaceable-frame backpressure. Its Herdr side
channel provides the closest architectural match for Prism.

Keep this proof local, static, and limited to one image. Keep animation, remote
bulk transport, foreign Kitty emulation, and a general canvas outside its
scope. Evaluate those topics through separate design decisions only when
evidence establishes a need.

### Scope and evidence

The analysis answers these questions:

- How does `terminal-browser` move Chromium pixels to a terminal that supports
  the Kitty graphics protocol?
- How does it detect support, choose a transport, place images, limit work,
  operate through tmux, and retire buffers?
- Which mechanisms fit Prism's rich surface, mux ownership, classic fallback,
  and quiet-idle constraints?
- Which small user task justifies raster content in the rich fabric?

The source review uses the exact local `terminal-browser` checkout at the
commit shown above. The analysis cross-checks protocol behavior against the
official Kitty specification and uses focused Rust tests as supporting
evidence. It does not claim a live compatibility matrix across Kitty, Ghostty,
tmux, Secure Shell (SSH), or Prism.

## Key findings

### The renderer sends one composite canvas

`terminal-browser` creates an offscreen Chromium window and combines the
browser frame with React-defined interface elements in one Rust canvas. The
terminal backend receives the complete red-green-blue-alpha (RGBA) canvas.

The producer uses several platform-specific frame sources:

- On Linux, it prefers a patched shared-memory frame unless
  `TERMINAL_BROWSER_SHM=0` disables that path.
- On macOS, it retains a shared texture.
- On other paths, it owns a bitmap copy.

Before the producer passes a frame to Rust, it checks the pixel format, content
origin, stride, size, and damaged region. If Rust retains a shared-memory
buffer, the release callback runs when Rust drops that buffer. If validation
rejects the frame, the callback runs immediately.

Damage information reduces conversion and composition work. It does not reduce
the final Kitty transmission. For every changed composite frame, the Kitty
backend sends the complete current canvas.

### Capability detection uses an active query

`terminals/src/graphics.ts` represents graphics support as `supported`,
`unsupported`, or `unknown`. On a real terminal, it performs this sequence:

1. It enables raw input mode.
2. It sends a 1-by-1 Kitty graphics query with image ID `4207`.
3. It sends the primary device attributes query.
4. It waits up to 500 milliseconds for `Gi=4207;OK`.
5. It limits the response buffer to 1,024 bytes.
6. It removes its listener and restores the original input mode.

When tmux is present, `terminal-browser` wraps the same query in a tmux
pass-through envelope. It queries pane pixel geometry separately with
`CSI 14 t` and another 500-millisecond timeout.

Two behaviors weaken this otherwise useful pattern:

- `TERMINAL_BROWSER_SKIP_GRAPHICS_CHECK` can force a positive result.
- If the query returns `unknown`, a recognized terminal name produces a
  positive result.

For Prism, keep query-first and fail-closed negotiation. If the host does not
send an affirmative, correlated feature grant, do not send image data.
Terminal names can improve an error message, but they must not create a
capability grant.

### The mailbox keeps only replaceable raster state

The owned-bitmap producer coalesces paint events until the next event-loop
turn. It keeps the newest bitmap, combines compatible damage rectangles, and
marks the entire surface as damaged after a size change.

The Rust `SurfaceMailbox` applies the same policy at the engine boundary:

- It keeps one pending slot for each surface ID.
- A newer frame replaces an unpresented frame.
- It combines damage from the replaced frame and the replacement.
- Unknown or full damage makes the combined damage cover the full surface.
- It recycles an owned buffer after a dropped or presented frame.
- It records submitted, coalesced, presented, and converted-row counters.

Surface conversion changes only the damaged blue-green-red-alpha (BGRA)
region to RGBA. The first frame and each resized frame use full damage. If the
producer omits damage information, the converter compares the full source and
reports no change for an identical frame.

This latest-only policy fits an explicitly replaceable image. It does not fit
ordered logs, diagnostics, history, or collection updates. For those semantic
messages, Prism must preserve order and request a new snapshot after a gap.

The browser controller also limits hidden, unpinned pages to 4 frames per
second and can reduce render scale from a maximum-pixel setting. These producer
controls reduce work, but they do not replace host-side limits. Treat every PTY
child as untrusted, even when it voluntarily reduces its frame rate or size.

### Kitty encoding uses bounded chunks

`engine/crates/pixel-core/src/kitty.rs` implements the direct Kitty backend:

- It identifies the pixel format as 32-bit RGBA with `f=32`.
- It compresses inline bytes with zlib level 1 and `o=z`.
- It encodes the compressed bytes as base64.
- It limits each payload chunk to 4,096 bytes.
- It puts action, dimensions, transport, image ID, placement, quiet mode, and
  continuation state in the first chunk.
- It puts only continuation state in later chunks.
- It wraps each complete APC chunk separately when tmux relays the output.

For direct placement, the backend uses the cursor and `C=1`, which prevents
cursor movement. For relayed placement, it uses Kitty virtual placement with
`U=1` and a grid of U+10EEEE placeholder characters. It stores the image ID in
the foreground color and uses row and column diacritics to address cells. The
implementation limits the grid to its 297 available row and column marks.

The Kitty specification confirms that payload chunks have a 4,096-byte limit,
only the first chunk includes full metadata, and the terminal displays an
image after it receives the complete sequence. The specification also
requires inline data when the application and terminal do not share a file
system.

### Transport selection probes the actual medium

At startup, the terminal backend selects a transport in this order:

1. It probes a temporary frame file for up to 300 milliseconds.
2. If the file probe fails, it probes Portable Operating System Interface
   (POSIX) shared memory for up to 300 milliseconds.
3. If both local probes fail, it sends compressed inline APC chunks.

You can override the result with
`TERMINAL_BROWSER_FRAMES=file|shared|shm|inline`. The file and shared-memory
probes ask the terminal whether it can open that specific medium. This test is
more reliable than terminal-name detection because a local process, SSH
client, container, and mux might not share a namespace.

The file transport uses eight files in a memory-mapped ring. It creates files
with mode `0600`, rewrites each slot in turn, creates a new generation after a
size change, and removes the files during cleanup. The POSIX shared-memory
transport also uses an eight-slot ring. Its names include process and terminal
identity.

Inline transport applies a default 3 MB/s frame budget. Large frames add up to
200 milliseconds of rate-limit delay. Named file and shared-memory transports
do not use this byte-budget delay.

For Prism, do not accept an arbitrary path from an untrusted child. Let the
host create an authenticated, generation-scoped local endpoint and a private
runtime directory. A descriptor-based transport provides a stronger option
because it avoids path lookup after validation.

### tmux support uses explicit framing and implicit configuration

`terminal-browser` gives the multiplexer precedence over its containing
terminal. A separate wrapper type encloses each Kitty chunk in a
`DCS tmux;` pass-through sequence and doubles each escape byte. Unit tests
verify the exact wrapper bytes.

Keep the explicit wrapper abstraction and byte-exact tests. Do not copy the
launcher behavior that changes tmux settings automatically. The launcher
enables pass-through, focus events, extended keys, Control Sequence
Introducer-u (CSI-u) format, and a client terminal feature. Prism should report
missing configuration instead of changing a user's mux state as a launch side
effect.

### Image cleanup lacks complete isolation

The backend places each changed frame inside DEC private mode `2026`
synchronized output. If the canvas shrinks, it deletes the image, clears the
screen, and resets placeholder state. It then sends the complete current
canvas through the selected transport. The relayed path redraws its placeholder
grid only when the cell dimensions change.

During shutdown, the backend removes shared-memory slots, deletes its Kitty
image, restores input and reporting modes, shows the cursor, leaves the
alternate screen, flushes output, and restores terminal settings.

Two direct-path choices create isolation risks:

- The backend always uses image ID `1`.
- Direct deletion uses `d=A`, which deletes and frees all images. The relayed
  path scopes deletion to its image ID with `d=I`.

For Prism, use generation-scoped asset identities and scoped deletion on every
path. A pane or child must not remove raster state that belongs to another
pane or child.

### The Herdr side channel best matches Prism

When `HERDR_PANE_ID` and `HERDR_SOCKET_PATH` exist, the terminal backend asks
`pane.graphics.info` for a transport. It accepts only
`file_frame_transport=direct-kitty`. The response supplies a host-owned frame
directory and exact cell dimensions. The client then opens a
`pane.graphics.stream` for a named layer.

For each frame, the client writes one of three mapped files and sends metadata
that includes:

- RGBA format and pixel dimensions.
- The file path.
- A monotonically increasing sequence.
- A revision.
- The viewport origin and cell-grid extent.

After a size change, the client increments the file generation and retires the
old ring. It removes retired files only after the host accepts a frame from the
new generation. This rule prevents the host from opening a removed path or
reading resized data through an old name. If the side channel fails, the
client uses the normal terminal backend and retries with exponential delays
from 1 to 10 seconds.

This model aligns with Prism's direct and mux architecture because it
separates a small control channel from local bulk data. However, do not copy
these weaknesses:

- `present` waits synchronously for each acknowledgment, and its socket read
  timeout reaches 12 seconds.
- The acknowledgment check looks only for a result and the absence of an
  error. It does not match the sequence or revision.
- The implementation scans strings for JavaScript Object Notation (JSON)
  fields instead of using a bounded, typed parser.
- Production rendering does not stop when `pane_visible` is false.

Keep asset input and output away from Prism's pseudoterminal (PTY) reader and
classic paint path. Use a bounded asset queue, an event-driven worker, and an
acknowledgment that names the exact generation, asset, revision, and sequence.

### Idle behavior is mostly event driven

The engine does not run a continuous frame loop when no content changes. It
blocks on a wakeable poll, creates deadlines only while animations run, and
emits a frame only when a scene or surface becomes dirty. Inline transport
adds rate-limit delay based on the encoded frame size.

Relayed operation adds one important exception: it limits every wait to 500
milliseconds so that it can poll for resize. This poll conflicts with Prism's
no-idle-tax requirement. Because Prism controls its mux server and viewer
protocol, represent resize, visibility, and asset updates as events.

### Runbook artifact preview provides a useful first application

Runbook already presents concurrent failure triage through a semantic task
rail, selected diagnostics, status values, actions, and a selectable PTY
transcript. Raster content adds value when text cannot represent an artifact
well. Useful examples include:

- A failed visual-test screenshot or an expected, actual, and diff image.
- A coverage heat map or compact chart.
- A benchmark plot.
- A flame graph or trace overview.
- A small generated artifact attached to the selected run.

Place the preview inside the selected-detail node. Do not use it as the
workspace root, and do not cover transcript rows or host chrome. Add a semantic
sibling that supplies the artifact kind, dimensions, timestamp, source task,
and useful alternative text.

In classic mode, print the same metadata and a safe artifact reference. Let
Runbook own any explicit action that opens an external viewer. The host must
not open content as an implicit side effect.

This application provides a useful proof for these reasons:

- It tests a task that benefits from pixels instead of adding decorative
  content.
- Static artifacts do not require an animation clock.
- One selected image keeps the cache and damage surface small.
- The semantic task and diagnostic tree remains useful when image delivery
  fails.
- Direct, detach, and reattach flows give the asset cache a concrete lifetime
  test.
- One fixture supports a comparison between inline preview and the current
  browser or file workflow.

### Classic Kitty compatibility remains separate

`terminal-browser` emits Kitty's `ESC _ G` APC namespace. It does not emit
Prism's `Prismattyc;` namespace and does not know about Prism's rich fabric. To run
it inside Prism, you need a Kitty graphics decoder and image state in the
classic terminal emulator, or a specified pass-through mode for nested Prism
sessions.

Do not include that compatibility work in the rich asset proof. Evaluate it as
a separate workstream with parser limits, scroll and erase behavior,
alternate-screen behavior, per-pane identities, deletion isolation, nesting
behavior, and a live `terminal-browser` fixture.

### The focused tests support the design analysis

The reviewed checkout produces these results:

- `cargo +1.96.1 test -p pixel-core --lib` passes 251 tests. The suite covers
  Kitty chunking, compression, wrappers, deletion, placeholders, Herdr frame
  rings, resize, process isolation, surface damage, terminal reply limits,
  frame files, shared memory, and wake behavior.
- `cargo +1.96.1 test -p pixel-node surface::tests` passes four tests. The
  suite covers latest-only frames, damage combination, unknown-damage
  promotion, and buffer recycling.

The repository pins Rust 1.93.1, but the local validation uses the installed
Rust 1.96.1 toolchain because the pinned toolchain does not install cleanly in
the review environment. Treat these results as research evidence, not as an
upstream release gate.

The `pixel-terminals` JavaScript tests do not run because the local package has
no `node_modules` directory and `tsc` is unavailable. The review inspects those
sources without installing dependencies solely for this analysis. The
`terminal-browser` checkout remains clean because the analysis does not modify
it.

No live Kitty, Ghostty, tmux, SSH, or Prism visual session contributes to this
evidence. Unit tests do not prove cross-terminal rendering, tmux configuration,
shared namespaces, or Prism compatibility.

## Recommendations

### Use separate semantic and raster planes

Keep Prism's semantic tree and classic transcript authoritative. Carry only
negotiation, metadata, references, commit status, acknowledgment, rejection,
and bounded failure records through the APC control plane. Move pixel bytes or
an already-open descriptor through a separate authenticated local bulk plane.

Use this conceptual flow:

```text
Runbook model and prismattyc-rich-client
  | APC control: node, asset identity, revisions, and alternative text
  | local bulk plane: frame bytes or descriptor, followed by commit
  v
direct host or mux server
  | validate, retain or copy, acknowledge, and keep a bounded asset cache
  v
viewer-local projection
  | resolve pixel size, cell rectangle, scale, clip, and damage
  v
prismattyc-host CPU raster and present path

ordinary PTY text -----------------------> authoritative classic grid
```

Because `prismattyc-host` owns the window framebuffer, sample an accepted RGBA asset
into the existing CPU raster and presentation path. Do not use Kitty commands
as the internal cache format or output requirement. Keep optional GPU
presentation outside this proof.

### Negotiate image and transport capabilities independently

Use separate feature grants for semantic image references and the available
bulk transport. Working names can include:

- `rich.image.rgba.v1`, which allows a semantic node to reference one bounded
  RGBA8 asset with alternative text and a fit policy.
- `transport.rich_asset.local_file.v1`, or a descriptor-based equivalent,
  which confirms an authenticated local bulk plane.

Do not use the existing generic `canvas` name. ADR-0014 leaves canvas support
unadvertised, and Prism's hybrid-rendering policy requires an explicit feature
before an image protocol becomes part of the rich content model.

If Prism cannot grant a safe local bulk transport, decline raster support. Keep
the semantic detail and classic artifact text complete.

### Bind every asset to an exact identity

Include these fields in each asset reference:

```text
surface_generation
asset_id
asset_revision
frame_sequence
format
width
height
stride
byte_length
optional_damage_rectangle
replaceable
```

Reference the asset identity from the rich tree. Do not put the bytes in the
tree message. In the mux server, keep only the latest validated and bounded
asset for each referenced ID. Invalidate all assets when the PTY generation
changes. After a node no longer references an asset, release that asset when
no retained scene uses it.

Require each acknowledgment to name the exact surface generation, asset ID,
asset revision, and frame sequence. Only that acknowledgment permits slot
reuse or retirement. Treat a duplicate commit with the same content identity
as idempotent. If the same revision names different content, drop the asset
layer as a conflict. Ignore stale acknowledgments without changing ownership.

### Use an authenticated local bulk transport

For the initial proof, choose one of these local mechanisms:

- Pass a Unix file descriptor so that the host validates an already-open
  regular file and avoids path lookup.
- Let the host create a directory under `XDG_RUNTIME_DIR` with mode `0700`.
  Create generation-scoped slots with mode `0600`, and never follow an
  existing link.

Bind a host-generated token to the pane and PTY generation. Before you accept
an asset, validate the regular-file type, owner, dimensions, stride, exact
length, multiplication overflow, and configured byte limits. Prevent torn
writes through descriptor ownership, atomic rename, or a content digest.

For a mux pane, let the mux server own the validated asset. The server owns the
PTY generation and restores a bounded asset for a newly attached viewer. Give
the viewer only canonical cached bytes or a server-owned local handle. Keep
scale, clipping, visibility, and damage local to each viewer. One viewer must
not change another viewer's projection or the shared Runbook state.

Do not reuse a local path across SSH or container boundaries. If the endpoints
do not share a trusted namespace, decline the raster capability and preserve
the complete semantic and classic fallback.

### Limit backpressure and idle work

Allow one queued replaceable frame for each raster node. A newer frame can
replace it only when the producer marks the stream as replaceable. Combine
damage from every displaced frame. If any displaced frame has unknown damage,
mark the next accepted frame as fully damaged. Record every replacement in
observable counters.

Keep ordered tree, collection, log, history, and diagnostic messages under
ADR-0014's existing rules. If an ordered queue fills, reject the update and
request a new snapshot. Raster coalescing must not imply that Prism observes a
missing diagnostic revision.

Run asset input and output on a worker that does not block the PTY parser. The
parser validates bounded control metadata and then enqueues or rejects the
request. It must not wait for file input, decoding, copying, a viewer, or an
acknowledgment.

When the worker has no work, block on an event source. Static content must not
create a timer, poll, repeated layout, or periodic repaint. Represent
visibility and resize changes as events.

### Apply narrow proof limits

Advertise and enforce these experimental limits:

- One live image node in the Runbook workspace.
- RGBA8 input only, with no animated formats or general image decoder.
- A maximum size of 2,048 by 1,024 pixels.
- A maximum committed frame size of 8 MiB.
- A maximum of 16 MiB of retained raster data for each PTY generation.
- One in-flight commit and one replaceable pending frame for each asset.
- No accepted frame when its node is absent, clipped to zero, stale, or outside
  the current surface generation.

Treat these values as prototype limits, not public guarantees. Use direct and
mux measurements to decide whether the feature remains useful within tighter
limits.

### Contain untrusted input and failures

Treat every image source as an untrusted PTY child, even when it uses a local
file. Validate and test these cases:

- Overflow in `width * height * bytes_per_pixel`, stride, offsets, and damage
  calculations.
- A file that changes type, owner, size, or content between validation and
  input.
- Symlinks, sockets, devices, directories, and paths outside the host-created
  root.
- Stale generation, asset, revision, sequence, node, and viewer references.
- Memory pressure across multiple panes and detached mux viewers.
- Producer termination before commit, during copy, and before
  acknowledgment.
- Viewer termination or detach while a frame remains in flight.
- A producer flood that repeatedly replaces the pending frame.
- Alpha or pixel data that attempts to cover transcript cells or host chrome.

Avoid compressed input in this proof so that decompression bombs remain out of
scope. If validation fails, drop only the image asset or reference. Record one
bounded degradation and keep the semantic workspace, classic transcript,
child, mux, and other panes active. Rate-limit repeated invalid commits or
revoke only the raster capability for the affected generation.

### Require evidence at each acceptance gate

Use these gates before you accept the proof:

| Gate | Required evidence |
|---|---|
| Query-first fallback | Prism sends no image bytes and opens no bulk connection before an affirmative feature grant. A timeout, malformed reply, or unsupported version keeps useful classic output. |
| Static usefulness | A recorded Runbook visual-diff test shows that inline preview reduces task time or avoids the external-viewer step. |
| Bounds | Dimension, stride, length, retained-byte, queue, damage, and multiplication-overflow tests fail closed. |
| File isolation | Prism rejects symlinks, devices, incorrect owners, path escapes, replacement races, and stale generations. |
| Acknowledgment ownership | A producer cannot reuse a slot before the matching acknowledgment. A stale or incorrect acknowledgment cannot release it. |
| Backpressure | A flood keeps memory bounded, the latest replaceable frame wins, damage carries forward, and counters account for every submission. |
| PTY progress | A stalled worker or missing acknowledgment does not delay classic output, input, resize, or pane closure. |
| Quiet idle | A static preview adds no timer, poll, repeated layout, or presentation. Direct and mux paths stay within existing idle gates. |
| Geometry | The image stays inside its tree placement and pane content. Transcript, selection, caret, and host chrome remain authoritative. |
| Direct and mux parity | The same semantic fixture, theme, scale, and geometry produce equivalent pixels through the direct host and mux viewer. |
| Detach and reattach | The mux keeps one bounded, validated asset and restores it for a new viewer without rerunning the task or accepting stale viewer state. |
| Multiple viewers | Viewer-local scale, clipping, and visibility do not change the shared asset or another viewer's projection. |
| Remote fallback | Without a shared namespace, Prism declines the capability and provides complete semantic and classic output without a path read or hang. |
| Cleanup | Node removal, generation end, pane closure, client failure, and server shutdown release only the resources that the affected asset owns. |
| Rendered behavior | Direct and mux screenshots confirm image fidelity, narrow resize, clipping, fallback, and two themes. |

### Deliver the proof in measured stages

1. Define the semantic image reference, identity fields, limits,
   acknowledgment, rejection, and classic fallback in an ADR amendment and
   protocol fixtures. Do not transmit pixels at this stage.
2. Implement one local RGBA8 artifact in the direct windowed host. Separate
   worker input from the PTY path, and record idle counters.
3. Add bounded mux-server ownership, reattach restoration, two viewer
   projections, exact acknowledgment, and cleanup.
4. Run a Runbook visual-diff fixture and compare it with opening the external
   artifact.
5. Keep the feature only if it passes the usefulness, isolation, progress,
   parity, cleanup, and quiet-idle gates.
6. Evaluate classic Kitty compatibility as a separate workstream.

### Resolve the remaining design questions

Before you create implementation tickets, answer these questions:

- Does every supported local direct and mux path support Unix descriptor
  passing, or does the proof require a private-directory transport?
- Does the mux server copy accepted pixels, retain an immutable mapped file, or
  give viewers a server-owned descriptor?
- Does the semantic node use `image`, `artifact`, or a narrower name such as
  `rgba_asset`?
- Which fit policies does the proof require beyond contain and clip?
- How can a user select and copy alternative text without confusing it with the
  underlying diagnostic selection?
- Does one static artifact improve the Runbook workflow enough to justify the
  complexity, or does an OSC 8 link to an external file remain sufficient?
- Which memory limit remains safe at the maximum pane count and across
  detached mux sessions?

## Additional resources

### Prism design references

- [Rich TUI next phase](rich-tui-next-phase.md) describes the practical rich
  experience and its planned feature set.
- [ADR-0014: Rich surface v2 fabric](adr/0014-rich-surface-v2-fabric.md)
  defines ordering, snapshot, negotiation, and fallback rules.
- [Hybrid rendering](hybrid-rendering.md) separates semantic rich content from
  classic terminal graphics.
- [Capability transport matrix](capability-transport-matrix.md) records the
  trust and transport boundaries for direct, mux, nested, and remote paths.

### Reviewed `terminal-browser` sources

The following links point to the exact reviewed commit:

- [`terminal-browser` product architecture](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/README.md#L59-L65)
- [Graphics probing and pane pixel geometry](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/terminals/src/graphics.ts#L5-L125)
- [Known-terminal fallback](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/terminals/src/detect.ts#L17-L30)
- [tmux configuration](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/terminals/src/terminals/tmux.ts#L50-L65)
- [Kitty encoding and placeholder tests](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/kitty.rs#L5-L300)
- [tmux byte wrapper](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/wrapper.rs#L1-L60)
- [Frame transport and terminal draw path](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/terminal.rs#L433-L668)
- [File and shared-memory cleanup](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/terminal.rs#L1108-L1252)
- [Herdr side channel](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/herdr.rs#L9-L164)
- [Latest-only surface mailbox](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-node/src/surface.rs#L19-L184)
- [Electron frame validation and bitmap coalescing](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/browser/src/page/paint.ts#L27-L174)
- [Engine idle and frame-rate path](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/engine/mod.rs#L609-L699)
- [Engine frame production path](https://github.com/zenbu-labs/terminal-browser/blob/9265a06fb875e5ed810359bbf007bd46d1156a3a/engine/crates/pixel-core/src/engine/mod.rs#L946-L1085)

### Protocol reference

- [Kitty terminal graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
  defines encoding, transport media, chunking, queries, Unicode placeholders,
  placement, and deletion.
