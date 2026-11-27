# Splitting PTY reader threads by activity

Status: **proposal from a time-boxed spike, not accepted direction.** Nothing here
changes shipped behavior.

Base: `origin/main` `e2d51317`. Line references are to that commit.

## Summary

Today `pmuxd` spawns **two** dedicated OS threads per live pane — one blocked in
`read(2)` on the PTY master and one blocked waiting for input to write — plus a
4 ms maintenance loop that scans every pane whether active or quiet. At 200
quiet panes on macOS, `pmuxd` holds **408 OS threads** (`8 + 2N`) and **609 open
file descriptors** (`8 + 3N`).

Moving quiet, unwatched Unix PTYs onto a single shared `kqueue`/`epoll` reactor
while keeping active or watched panes on dedicated reader threads cuts per-pane
reader threads from `N` to `1` without stalling hot panes or adding transitive
dependencies (`polling = "3.11.0"` is already in `Cargo.lock`, sharing
`rustix 1.1.6`).

**Recommendation: go on Unix (macOS and Linux); keep dedicated reader threads on
Windows in v1.**

- **Refined build estimate:** **15–19 agent-hours / 4–5 wall-clock days** across
  3 PRs (flag off by default in PRs 1–2; default-on flip plus measurements in
  PR 3).
- **Two companion findings from measuring live `pmuxd`:**
  1. **Writer threads account for the other half of `2N`:** moving only idle
     readers drops 200 quiet panes from `408` to `209` threads. A small
     follow-up (pooled or on-demand writers) is needed to bring idle pane
     threads all the way to `O(1)`.
  2. **Idle CPU comes from the 4 ms drain scan, not blocked reader threads:**
     blocked `read(2)` threads use ~0% CPU, while `prism-server-runtime`
     (`control.rs:7670-7683`) scans all `N` panes every 4 ms (`+0.02 ms/s` per
     quiet pane). Adding a small ready-pane queue alongside the reader split
     flattens idle CPU to `O(active)`.

## How `pmuxd` runs PTY I/O today

This note covers **server-side** PTY I/O inside `pmuxd` (`crates/prismattyc-mux`).
It is separate from `docs/design/render-thread.md`, which addresses the windowed
host's main-thread raster and present pipeline.

### Thread and file-descriptor inventory

- **Fixed daemon threads (8–9):**
  1. `main` waiting on `ShutdownGate` (`bin/pmuxd.rs:303`)
  2. `prism-control-accept` polling `UnixListener::accept` every 5 ms (`control.rs:7647,7833`)
  3. `prism-server-runtime` looping every 4 ms (`control.rs:7668-7683`)
  4. `attention` reconciler (`attention.rs:285`)
  5. `workspace::Checkpointer` (`workspace.rs:596`)
  6. `space_autosave::Worker` (`space_autosave.rs:313`)
  7. `activity_config::Watcher` (`activity_config.rs:97`)
  8. `activity_webhook` worker/projection (`activity_webhook.rs:295,304`)
  9. `pmux-checkpoint` when pane-log persistence is enabled (`pane_log_persist.rs:52`)
- **Per-client threads:** 1 `prism-control-client` thread per connected socket,
  capped at `MAX_CLIENTS = 64` (`control.rs:62,7815`).
- **Per-pane threads (`LivePane::spawn`, `live.rs:296-363`):**
  1. `prism-server-pane-{id}-write` (`live.rs:297`): blocks on
     `to_child_rx.recv()`, writes to `child_writer`, and exits if `write_all`
     or `flush` returns any error (`live.rs:299-306`).
  2. `prism-server-pane-{id}-read` (`live.rs:318`): allocates an 8 KiB buffer
     (`live.rs:320`), loops on `pty_reader.read(&mut buffer)`, reserves space in
     the per-pane 8 MiB `ByteBudget` (`pane_log.rs:19,74`), and sends chunks on
     a bounded `mpsc::sync_channel(64)` (`FROM_PTY_CAP`, `live.rs:34`).
- **Per-pane file descriptors on Unix (`portable-pty 0.9.0`, `src/unix.rs:352-368`):**
  `PtySession::spawn_config_with_fds` (`crates/prismattyc-emulator/src/lib.rs:2244-2284`)
  retains `master: Box<dyn MasterPty + Send>` (1 fd), calls `try_clone_reader()`
  (1 `dup` fd), and calls `take_writer()` (1 `dup` fd) = **3 FDs per pane**, all
  pointing to the same kernel open file description.
- **Drain loop (`control.rs:7670-7683`, `live.rs:1315-1339`):** every 4 ms
  (250 Hz), `prism-server-runtime` takes the plane lock and calls
  `LiveRuntime::drain()`, which iterates every pane calling
  `from_pty_rx.try_recv()` (up to `MAX_DRAIN_PER_PANE = 16` chunks per pane),
  followed by `maybe_persist_pane_logs()`, `retry_mail_nudges()`, and
  `check_pane_idle()`.

### Measured baseline (1 to 200 quiet panes)

Measured on macOS (18-core ARM64, 16 KiB pages, `pmuxd 0.3.76`, isolated socket
with `PMUX_PANE_LOG=off` and `ulimit -n 4096`). Each pane ran `/bin/sleep 300`
with empty scrollback; CPU was averaged over a 5 s quiet window after settling:

| Quiet panes (`N`) | OS threads | Open FDs | RSS (empty scrollback) | Idle CPU (`pmuxd`) |
| --- | ---: | ---: | ---: | ---: |
| 1 | 10 | 11 | 27.95 MiB | 7.95 ms/s (0.80%)* |
| 50 | 108 | 158 | 34.77 MiB | 3.98 ms/s (0.40%) |
| 100 | 208 | 308 | 42.78 MiB | 5.97 ms/s (0.60%) |
| 200 | 408 | 609 | 58.70 MiB | 7.94 ms/s (0.79%) |

*\*The 1-pane sample immediately after startup includes one-time daemon
initialization/plugin seeding; steady-state scaling from 50 to 200 panes is
strictly linear.*

Key per-pane scaling rates (50 → 100 → 200 quiet panes):

- **Threads:** exact `+2.00` threads/pane (`8 + 2N`).
- **FDs:** exact `+3.00` FDs/pane (`8 + 3N`).
- **RSS (empty scrollback):** `+163 KiB/pane` (empty 80×24 emulator +
  `LivePane` + faulted pages of 2 thread stacks + 8 KiB reader buffer + channel
  rings). On Linux, Rust's 2 MiB default thread stack also reserves 4 MiB of
  virtual address space per pane, and container cgroups (`pids.max`) or
  `RLIMIT_NPROC` count every thread against the process/task cap.
- **Idle CPU:** `+0.02 ms/s` per quiet pane (`+0.002%` of one core/pane),
  driven by the 250 Hz `drain_live()` + `check_pane_idle()` scan across all
  panes, not by the sleeping reader threads.
- **OS limits to respect:**
  - macOS shells default to `ulimit -n 256` (~80 panes at 3 FDs/pane unless
    raised), and `kern.tty.ptmx_max` defaults to `511`.
  - Adding permanent per-pane wake pipes to *idle* panes would push FD usage
    from `3N` to `5N`. Wake pipes must exist only on active dedicated-reader
    panes (0 extra FDs for idle panes).

## Windows and ConPTY portability

A shared `kqueue`/`epoll` reactor **does not carry over** to Windows ConPTY on
top of `portable-pty 0.9.0`:

1. **Synchronous anonymous pipes:** `portable-pty 0.9.0`'s
   `ConPtySystem::openpty` (`src/win/conpty.rs:14-24`) allocates the ConPTY
   input and output pipes via `filedescriptor::Pipe::new()`
   (`filedescriptor 0.8.3`, `src/windows.rs:421-446`), which calls Win32
   `CreatePipe` without `FILE_FLAG_OVERLAPPED`. Synchronous anonymous pipes
   cannot be registered with an I/O Completion Port (`CreateIoCompletionPort`),
   nor can they issue overlapped `ReadFile` calls.
2. **`polling` crate scope on Windows:** `polling 3.11.0`'s Windows backend
   (`iocp`/wepoll) polls WinSock sockets only, not anonymous or named pipes.
3. **Existing Windows ConPTY lifecycle contracts:** `LivePane` depends on
   `CancelSynchronousIo(self.reader_thread.as_raw_handle())` (`live.rs:417-424`)
   for `ChildExitState::Draining` (`live.rs:154-159,654-708`) and on the
   `Drop` helper thread that answers `\x1b[6n` cursor requests so `ClosePseudoConsole`
   does not hang (`live.rs:197-220`).

**Decision for v1:** isolate the reader-split behind a small `ReaderDriver`
abstraction in `crates/prismattyc-mux/src/`:

- `#[cfg(unix)]`: hybrid dedicated-thread + shared `kqueue`/`epoll` reactor.
- `#[cfg(windows)]`: retains the existing per-pane dedicated reader thread and
  `CancelSynchronousIo` exit-draining path unchanged.

**Future Windows option (if Windows fleet scale ever exceeds ~100 quiet panes):**
replace `portable-pty`'s `ConPtySystem` inside `prismattyc-emulator` with a
direct `CreateNamedPipeW(..., FILE_FLAG_OVERLAPPED)` +
`CreateFileW(..., FILE_FLAG_OVERLAPPED)` + `CreatePseudoConsole` wrapper and an
IOCP loop (`GetQueuedCompletionStatusEx`). That is a standalone emulator-crate
effort and should not block Unix fleet scaling.

## Proposed Unix design

### 1. FD ownership and the `O_NONBLOCK` hazard

On Unix, `UnixMasterPty`'s master fd, `try_clone_reader()` dup, and
`take_writer()` dup all refer to the **same** kernel open file description
(`struct file`). Verified on macOS: setting `O_NONBLOCK` on the reader dup
immediately makes the master fd and `UnixMasterWriter` non-blocking too.

If `O_NONBLOCK` is enabled while `live.rs:300` still calls plain
`child_writer.write_all(&bytes)`, any `WouldBlock` (for example, a stopped
child or a full PTY input queue) causes `write_all` to fail and **permanently
terminates the pane's writer thread**. Flipping `O_NONBLOCK` on and off during
promote/demote would also race with concurrent writes and reads.

**Clean fix:**

1. Add Unix-only `PtySession::take_reader_fd(&mut self) -> Result<OwnedFd>`
   and `PtySession::take_writer_fd(&mut self) -> Result<OwnedFd>` helpers that
   duplicate `self.master.as_raw_fd()` with `F_DUPFD_CLOEXEC` and consume the
   same one-shot reader/writer slots (`take_writer()` returns an opaque
   `Box<dyn Write + Send>` that does not expose `AsRawFd`, so polling `POLLOUT`
   on `WouldBlock` requires an owned descriptor). Two readers or writers can
   still never be created for one session.
2. Set `O_NONBLOCK` **once at spawn** on the Unix PTY master open file
   description and never toggle it at runtime.
3. Wrap the Unix writer fd in a small `NonBlockingWriter` helper: on
   `WouldBlock`, wait with `rustix::event::poll` (`POLLOUT`, bounded timeout or
   wake fd so drop/shutdown never hangs) before retrying the remaining slice;
   retry `EINTR`; treat `EPIPE`/`EIO` as normal disconnect. (Preserve
   `UnixMasterWriter`'s best-effort `\n` + `VEOF` on drop or leave child
   termination to `PtySession::kill_and_reap`, matching existing drop tests.)
4. Normalize Unix PTY EOF in one shared helper `read_pty_nonblocking(fd, buf)`:
   - `Ok(0)` or `Err(EIO)` → `ReadStep::Eof` (matches `portable-pty`'s
     `PtyFd::read` in `unix.rs:93-106`, where Linux reports `EIO` on slave
     close while macOS reports `Ok(0)`).
   - `Err(EAGAIN | EWOULDBLOCK)` → `ReadStep::WouldBlock`.
   - `Err(EINTR)` → retry.
   - Any other `Err(e)` → `ReadStep::Error(e)`.

### 2. Single-reader ownership and zero-loss handoff

Every Unix pane keeps the **same** `from_pty_rx: mpsc::Receiver<io::Result<PtyChunk>>`
and `ByteBudget` for its entire life. Only the producer holding
`ReaderOwnership { pane_id, fd: OwnedFd, tx: SyncSender<io::Result<PtyChunk>>, budget: ByteBudget, ready: Arc<ReadyPanes> }`
changes between two states:

```text
               quiet + unwatched + no lease + cooldown
  ┌───────────┐ ──────────────────────────────────────► ┌────────────┐
  │ Dedicated │                                         │   Shared   │
  │ (1 thread │ ◄────────────────────────────────────── │  (1 global │
  │  + wake   │  watch/attach, lease/write, output      │   reactor  │
  │   pipe)   │  burst, or non-parked backpressure      │   thread)  │
  └───────────┘                                         └────────────┘
```

**Invariant:** at every instant, `ReaderOwnership` (and its `OwnedFd`) is owned
by **either** the pane's dedicated thread **or** the shared reactor thread —
never both. No two threads ever call `read(2)` on the same PTY concurrently,
and every chunk travels over the same FIFO `from_pty_tx -> from_pty_rx` channel
in exact kernel read order.

#### Dedicated mode (active/watched panes)

- Runs a dedicated thread `prism-server-pane-{id}-read` holding
  `ReaderOwnership` plus the read end of a small close-on-exec wake pipe
  (`wake_r`, 2 FDs total, allocated only while `Dedicated`).
- Hot loop: drains non-blocking `read()` calls in a tight loop while bytes are
  available (checking an `AtomicU8` command word between chunks; **zero**
  `poll(2)` syscalls during sustained bursts).
- When `read()` returns `WouldBlock`, blocks in a 2-fd `rustix::event::poll`
  on `[pty_fd (POLLIN), wake_r (POLLIN)]`.
- On a chunk, calls blocking `budget.admit(...)` and `tx.send(...)` just like
  today, applying kernel backpressure to the child if the consumer lags —
  except it checks the command word after `send` returns (or before blocking on
  the next `poll`).
- **Graceful demotion (`Dedicated → Shared`):** control plane sets the pane's
  command word to `DEMOTE` and writes 1 byte to `wake_w`. The dedicated thread
  wakes immediately without `pthread_kill` or closing `pty_fd`, finishes any
  chunk already read (zero lost bytes), closes `wake_r`/`wake_w` (releasing
  both wake FDs), pushes `ReaderOwnership` onto the reactor's command queue,
  calls `poller.notify()`, and exits.
- **Shutdown / pane close while `Dedicated`:** control plane sets command word
  to `STOP`, writes 1 byte to `wake_w`, and drops `from_pty_rx`; the thread
  exits immediately even if blocked in `tx.send` (`Disconnected`) or `poll`.

#### Shared reactor mode (quiet/unwatched panes)

- One process-wide thread `prism-server-pty-reactor` built on `polling::Poller`
  (`polling = "3.11.0"`, already in `Cargo.lock`):
  - **macOS:** `kqueue` with `EVFILT_READ` (`PollMode::Level`) and built-in
    `EVFILT_USER` wakeup (0 pipe FDs for reactor wakeup).
  - **Linux:** `epoll` with `EPOLLIN` (`PollMode::Level`) and `eventfd` wakeup
    (1 FD total for the entire reactor).
- Maintains `HashMap<u64, ReactorPane>` keyed by `pane_id` (used directly as
  the `Poller` event key) and one shared 8 KiB read scratch buffer (saving
  `8 KiB × N` per-pane buffers).
- Processes a command queue (`Register`, `Promote`, `ResumeRead`, `Remove`)
  woken via `poller.notify()`.
- **Promotion (`Shared → Dedicated`):** always executed **by the reactor thread
  itself** (whether requested by the control plane or triggered internally by a
  burst):
  1. `poller.delete(&pane.fd)` (also satisfies `polling::Poller::add_with_mode`'s
     safety contract that registered descriptors remain open until deleted)
  2. Remove `ReactorPane` from the map (taking `ReaderOwnership`)
  3. Create the wake pipe and spawn `prism-server-pane-{id}-read` with
     `ReaderOwnership` (if `thread::Builder::spawn` ever fails under OS thread
     pressure, keep the pane safely on the reactor and log a warning — graceful
     degradation!).
  4. Because step 1 completes before step 3 starts, dual reads are impossible.

#### Non-blocking reactor rule (preventing head-of-line stalls)

This is the single most important correctness rule of the shared reactor: **the
reactor thread must never block on any one pane's `ByteBudget` or
`mpsc::SyncSender`**.

Today `ByteBudget::admit` (`pane_log.rs:50-81`) blocks on a `Condvar` when 8 MiB
is reserved, and `SyncSender::send` blocks when 64 chunks are queued. In
particular, the idle-parking design (`docs/design/park-idle-panes.md`)
deliberately stops draining a parked pane until unpark finishes, relying on
channel/budget backpressure to hold back the child. If the shared reactor
blocked on that pane, every other idle pane on the reactor would freeze.

**Solution:**

1. Add a non-blocking `ByteBudget::try_admit` (`try_reserve` returning `None`
   immediately instead of waiting on `available.wait`).
2. Before reading (or when `try_admit` / `try_send` says full), each
   `ReactorPane` may hold at most **one** already-read `pending: Option<PtyChunk>`
   slot and **pause its `Poller` read interest**
   (`poller.modify_with_mode(&fd, Event::none(pane_id), PollMode::Level)`).
3. While paused, the kernel PTY buffer holds back that one child while every
   other fd in the `Poller` continues draining with zero latency impact.
4. When `LivePane::drain` consumes chunks from a paused pane, it sends
   `ReactorCmd::ResumeRead(pane_id)` + `poller.notify()`. The reactor flushes
   `pending` via `try_send` and re-arms `Event::readable(pane_id)` — or
   promotes the pane if it is not parked and is bursting.

#### EOF / HUP consistency across macOS and Linux

- Empirically verified on macOS: closing the PTY slave raises `EVFILT_READ`
  with `EV_EOF`, and the subsequent non-blocking `read(2)` returns `Ok(0)`.
- On Linux `epoll`, closing the PTY slave raises `EPOLLHUP` (often with
  `EPOLLIN`), and `read(2)` returns `Err(EIO)` once kernel buffers are empty.
- On `ReadStep::Eof` (or `ReadStep::Error`), the reactor immediately deletes
  the fd from `poller`, drops the `OwnedFd`, and pushes `PtyChunk::eof()` (if
  the channel is temporarily full, holds `eof_pending = true` until
  `ResumeRead` delivers it — never silently drops EOF!). `LivePane::drain` and
  `observe_death` (`live.rs:428`) see the exact same `PtyChunk::eof()` edge and
  emit `PaneEvent::Exited` and `CellExitNotice` once.

### 3. Ready-pane drain optimization (flattening idle CPU)

Because `LiveRuntime::drain()` (`live.rs:1315`) currently scans every pane
every 4 ms, moving readers to a shared reactor alone will not drop the
`+0.02 ms/s/pane` idle CPU slope. Adding a lightweight `ReadyPanes` set
(`Mutex<HashSet<u64>>` + `AtomicBool` fast-path, marked by both dedicated
readers and the shared reactor whenever a chunk or EOF is queued) lets
`LiveRuntime::drain()` iterate only panes with pending output (plus Windows
panes in `ChildExitState::Running`/`Draining`). Idle panes do **zero** work in
the 4 ms tick.

### 4. Promotion and demotion triggers (with hysteresis)

All panes start **`Dedicated`** at spawn so initial shell banners, prompt
setup, and short commands never pay a cold promotion.

#### Demote (`Dedicated → Shared`) — all must hold

1. **Idle duration:** no PTY output (`last_output_at_ms`, `control.rs:2449`) and
   no `WritePane` input (`last_input_at_ms`, `control.rs:2441`) for at least
   `demote_idle` (default **30 s**, configurable). This sits well below the
   5-minute memory-parking delay (`docs/design/park-idle-panes.md`) and below
   the 60 s `PaneIdle` event (`PANE_IDLE_TRIGGER_MS`, `control.rs:369`), so
   quiet background panes shed their reader threads promptly and every parked
   pane is already demoted.
2. **Not watched:** the pane is not in any client's `client_views` /
   `client_window_views` / `client_pane_focus` (`control.rs:2411-2413,2456`)
   and has zero open `SubscribePane` streams (sharing the per-pane watcher
   count introduced by `docs/design/park-idle-panes.md`).
3. **No controller lease:** `pane.controller.is_none()` (`domain.rs:98`).
4. **Child running:** `child_alive()` is true (exited panes have no reader
   thread anyway).
5. **Cooldown / hysteresis:** at least **10 s** have elapsed since this pane's
   last promotion, preventing thread-spawn churn on panes with noisy 1–5 s
   activity spikes.

#### Promote (`Shared → Dedicated`) — any triggers immediate promotion

1. **Client watch or attach:** a client attaches to the pane's session/window,
   focuses the pane, or opens `SubscribePane`.
2. **Lease or input:** `AcquireLease`, `TakeoverLease`, or `WritePane`
   (including mail/command injection) targets the pane, anticipating immediate
   echo and output.
3. **Output burst:** the shared reactor reads `>= 4` chunks or `>= 16 KiB` from
   the pane within a `250 ms` sliding window, or hits backpressure on an
   unparked pane. Small periodic output (a clock prompt, keepalive, or status
   tick once a second) stays on the reactor without spawning a thread.
4. **Unpark:** when a parked pane receives output, the reactor delivers the
   first chunk (triggering unpark) and pauses read interest until unpark
   completes; if output continues past the burst threshold after unpark, it
   promotes immediately.

### 5. Feature gating and configuration

Per trunk policy, new behavior ships behind an opt-in config flag that is **off
by default** in the implementation PRs:

- `[mux] split_pty_readers = false` (env override
  `PRISMATTYC_EXPERIMENTAL_SPLIT_PTY_READERS=1`)
- `[mux] reader_demote_idle_secs = 30` (`0` disables demotion)
- `[mux] reader_promote_cooldown_secs = 10`

Turning `split_pty_readers` on by default on Unix is its own small trunk PR
after soak and measurements.

## Risks and mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| `O_NONBLOCK` breaks `UnixMasterWriter::write_all` | Writer thread exits permanently on `WouldBlock` | Ship `NonBlockingWriter` (`POLLOUT` retry) in PR 1 before any reactor code exists; unit-test against a full PTY buffer. |
| Blocking `ByteBudget::admit` or `SyncSender::send` inside shared reactor | One slow/parked pane freezes all idle panes | Reactor uses `try_admit` + `try_send` + 1-chunk pending slot + `Poller` read-interest pause; never blocks. |
| Dual-reader race during promote/demote | Lost, duplicated, or reordered bytes | Single-owner `ReaderOwnership` token; dedicated thread hands fd to reactor on demote; reactor deletes fd from `Poller` before spawning dedicated thread on promote. |
| Thread-spawn thrashing on periodic heartbeats | CPU/allocator churn | Require burst threshold (`>= 4` chunks or `>= 16 KiB` in `250 ms`) to auto-promote on output, plus 10 s post-promotion cooldown. |
| FD exhaustion on macOS (`ulimit -n 256`) | `openpty` or pipe creation fails at ~80 panes | Allocate wake pipes only for `Dedicated` panes (0 extra FDs on `Shared` panes); document raising `RLIMIT_NOFILE` at daemon start as a related ops note. |
| `thread::Builder::spawn` fails on promotion under `RLIMIT_NPROC` | Dropped pane output | Keep pane on the shared reactor on spawn failure and log once; output continues uninterrupted. |
| macOS vs Linux EOF differences (`EV_EOF` + `Ok(0)` vs `EPOLLHUP` + `EIO`) | Missed exit notice or busy-spin | Normalize `Ok(0)` and `EIO` to `ReadStep::Eof`; delete fd from `Poller` immediately on EOF/error; test on both macOS and Linux CI. |
| Parked-pane unpark + reactor backpressure | Dropped first chunk or stalled reactor | Pause `Poller` read interest when `from_pty_tx` is full; `LivePane::drain` resumes interest once unpark drains chunks. |
| Remaining `N` idle writer threads | 200 quiet panes still hold ~209 threads until writers are pooled | Explicitly documented as a separate follow-up ticket (`~6–8` agent-hours) so expectations for thread-count savings are exact. |

## Proof plan (proving hot panes do not stall)

Every item below runs in automated tests and/or a reproducible benchmark before
turning `split_pty_readers` on by default:

1. **Hot-pane stall isolation benchmark (200 quiet + 2 hot):**
   - Spawn 200 quiet panes (`/bin/sleep`) and wait for all 200 to demote to
     `Shared` (assert `pmuxd` reader threads == 1 reactor + 2 hot = 3).
   - Flood both hot panes with 32 MiB of structured output simultaneously.
   - Compare hot-pane throughput (MiB/s) and p99 `drain_live` latency against a
     2-pane baseline (no quiet panes). **Gate:** hot-pane throughput within 5%
     of baseline and no `drain_live` stall > 15 ms.
2. **Reactor non-blocking / backpressure isolation test:**
   - Demote 100 panes onto the shared reactor.
   - Deliberately stop draining 1 pane's `from_pty_rx` (simulating a parked
     pane awaiting unpark) while its child floods > 1 MiB into its PTY until
     its channel fills and its `Poller` interest pauses.
   - Write a marker to 10 other panes on the same reactor and assert all 10
     deliver within `< 20 ms` p99.
   - Resume draining the stalled pane and verify zero dropped or reordered
     bytes.
3. **Promote/demote byte-integrity torture test:**
   - Child streams 10 MiB of numbered lines (`00000000`..`N`) while a harness
     forces promote/demote transitions every 20 ms (100+ transitions).
   - Assert exact SHA-256 / byte-for-byte equality against the expected stream,
     with zero duplicates, gaps, or reordering.
4. **EOF / exit edge-case tests (macOS `kqueue` + Linux `epoll`):**
   - Child exits while `Shared`, while mid-`Demote`, and while mid-`Promote`.
   - Assert `PaneEvent::Exited` and `CellExitNotice` fire exactly once, no
     busy-spin occurs, and open FD count returns to baseline after pane close.
5. **Non-blocking writer backpressure test:**
   - Stop a child (`SIGSTOP`), write past the kernel PTY input buffer until
     `WouldBlock` triggers, resume (`SIGCONT`), and verify all bytes arrive in
     order without the writer thread exiting.
6. **Windows regression gate:**
   - Existing Windows ConPTY suite (including
     `exiting_child_waits_for_inflight_reader_publication` and pane close
     tests) stays green with zero behavioral change.

## Build plan and effort estimate

Calibrated against recent `prismattyc-mux` PRs (multi-file runtime + control
plane + cross-platform tests, typically 4–8 agent-hours and 1–2 wall-clock days
per PR including review and CI). This refines the earlier PI-close median
(`16 agent-hours / 4 wall-clock days`).

| PR | Scope | Agent-hours | Wall-clock days |
| --- | --- | ---: | ---: |
| **PR 1** | `PtySession::take_reader_fd` + `take_writer_fd` (Unix) + `O_NONBLOCK` at spawn + `NonBlockingWriter` (`POLLOUT` retry) + `ReaderDriver` abstraction (`Dedicated` with wake pipe on Unix; unchanged passthrough on Windows) + `WouldBlock` & graceful-stop tests | 6–7 h | 1.5–2 d |
| **PR 2** | Shared `polling` (`kqueue`/`epoll`) reactor (`prism-server-pty-reactor`), `ByteBudget::try_admit` + interest pause, promotion/demotion policy & hysteresis behind `[mux] split_pty_readers = false`, `ReadyPanes` drain set, and the stall/backpressure/torture/EOF proof suite | 7–9 h | 2 d |
| **PR 3** | Flip `[mux] split_pty_readers = true` on Unix, record macOS + Linux 200-quiet + 2-hot scaling numbers in `docs/design/`, and verify Windows CI | 2–3 h | 1 d |
| **Total** | **3 trunk PRs** | **15–19 h** | **4.5–5 d** |

### Recommended build tickets

1. **Build: split Unix PTY reader threads by activity** (PRs 1–3 above;
   **15–19 agent-hours / 4.5–5 wall-clock days**).
   - Can start independently of idle memory parking (`docs/design/park-idle-panes.md`);
     shares the per-pane watcher count if parking lands first, or introduces
     the minimal watcher counter itself so parking can reuse it.
2. **Optional follow-up: pool or lazily spawn idle PTY writer threads**
   (**6–8 agent-hours / 1.5–2 wall-clock days**).
   - Eliminates the remaining `N` idle `prism-server-pane-{id}-write` threads
     so 200 quiet panes drop from `~209` threads to `~10` threads (`O(1)`),
     completing the thread-count reduction at fleet scale.

## Out of scope

- Live PTY fd handoff across `pmuxd` restarts.
- Disk snapshots / idle emulator memory parking (covered in
  `docs/design/park-idle-panes.md`).
- Replacing `portable-pty`'s Windows ConPTY backend with custom overlapped
  named pipes in v1.
- Pooling per-pane writer threads in v1 (tracked as the optional follow-up
  above).

## Open questions before build

1. **Watcher counter coordination:** should PR 1 of idle parking or PR 2 of the
   reader split own adding the per-pane `SubscribePane` watcher count on
   `ControlPlane`? (Whoever lands first adds it; the second reuses it.)
2. **Default `demote_idle` (30 s) and burst threshold (4 chunks / 16 KiB in
   250 ms):** confirm whether 30 s feels right for interactive tab-switching
   workflows, or whether watched-in-layout tabs (which stay `Dedicated` anyway)
   make 30 s completely invisible.
3. **Follow-up writer pooling:** decide whether to queue the writer-thread
   follow-up right after PR 3 so 200 quiet panes reach `~10` total threads
   instead of `~209`.
