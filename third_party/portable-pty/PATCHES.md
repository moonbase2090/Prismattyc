# Local patches to portable-pty 0.9.0

Vendored from crates.io, version 0.9.0, unchanged except for the items below.
The original license is in LICENSE.md.

1. `CommandBuilder::child_fd(source, target)` (unix only). Records a mapping from
   an open descriptor in the parent to a descriptor in the child.
2. `unix.rs` spawn path: in the child, after fork and before exec, each mapping is
   applied with `dup2` (or a clear of close-on-exec when source and target are the
   same). This happens before `close_random_fds`, and that close keeps the target
   numbers open. The existing spawn path is unchanged when no mapping is given.
3. `unix::adopt_master(fd)` (unix only). Wraps an already open PTY master
   descriptor, for example one a process kept across its own exec, in the same
   master type `openpty` returns. It sets close-on-exec and reads the slave path
   with `TIOCPTYGNAME` (macOS) or `ptsname_r` (other unix).
