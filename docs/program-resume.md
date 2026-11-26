# Program resume (OSC 7577)

A program running in a pane can tell the pmux server how to start it again
after the server restarts: after **Update and restart**, a `pmux restart`, or
a login restore. The pmux server then reopens the pane in its shell and runs
the command the program reported, instead of guessing it from the process
list. Resuming starts a new process. Nothing is handed over from the old one,
and the program restores its own state from whatever it saved (for example a
Neovim session file).

The pmux server implements the OSC 7577 program resume protocol. It is on by
default on Linux and macOS.

## What a program does

1. It checks for `OSC7577_TOKEN` in its environment. If the variable is not
   set, the terminal does not take reports and the program does nothing.
2. It builds a report body: `pid=PID:time=SECONDS` plus either
   `argv=` (each argument as standard base64 of UTF-8, joined by `,`) and an
   optional `cwd=` (base64 of an absolute directory), or `clear=1` to remove
   its record.
3. It appends `:mac=` and the lowercase hex SHA-256 of the body, a `:` and
   the token.
4. It writes `ESC ] 7577 ; body ESC \` (or BEL) to its terminal.

A program reports again whenever its resume command changes, and sends
`clear=1` when the user quits it on purpose. It does not send `clear=1` when
it exits because the terminal went away. A program that starts other
programs removes `OSC7577_TOKEN` from their environment. A shell passes it
on. A launcher that runs a child with less authority (as another user, in a
chroot or container, or in a sandbox) must remove `OSC7577_TOKEN` from that
child's environment.

This shell wrapper reports for the program it starts, then replaces itself
with it:

```sh
#!/bin/sh
# resume-wrap PROGRAM [ARGS...]
[ -n "$OSC7577_TOKEN" ] || exec "$@"
argv=
for arg in "$@"; do
  argv="$argv${argv:+,}$(printf '%s' "$arg" | base64 | tr -d '\n')"
done
msg="pid=$$:time=$(date +%s):argv=$argv:cwd=$(pwd | tr -d '\n' | base64 | tr -d '\n')"
mac=$(printf '%s:%s' "$msg" "$OSC7577_TOKEN" | shasum -a 256 | cut -c1-64)
printf '\033]7577;%s:mac=%s\007' "$msg" "$mac" >/dev/tty
unset OSC7577_TOKEN
exec "$@"
```

`printf` is a shell builtin, so the token does not appear in the process list.

## What the pmux server checks

The pmux server gives the first process of each local pane its own random
token. It never passes on a token it was given by the terminal it runs in,
and a pane whose program is `ssh`, `mosh` or `rsh` gets no token, so the
token does not leave the machine. Do not add `OSC7577_TOKEN` to an ssh
`SendEnv` list.

As the protocol intends, any process of the same user that holds the
pane's token is trusted to report. A report is **ignored**, and changes
nothing, when:

- its MAC is missing, not last, or wrong for the pane's token;
- its time is more than 60 seconds from the clock, or older than the last
  report accepted from the same pid;
- it is longer than 2048 bytes, counting the escape sequence;
- any process in the pane (its first process or a descendant) runs as
  another user, runs in a sandbox, or has another root directory or
  namespace than the pmux server, because such a process may hold the token
  without the user's authority. The same applies when the pane's processes
  cannot all be listed and checked. The pmux server logs this once for the
  pane.

What is detected:

- **macOS:** another user, and a sandbox (as `sandbox_check` reports it, for
  example a process started under `sandbox-exec` or an App Sandbox app). A
  changed root directory is not detected.
- **Linux:** another user, a seccomp filter (as in many containers and
  sandboxes), and a root directory or user, mount or pid namespace different
  from the pmux server's. Other confinement, such as an AppArmor or SELinux
  profile or a Landlock ruleset, is not detected.
- A process that confines itself after starting, without any of the above,
  is not detected. Removing the token from restricted children is what the
  protocol relies on.

A report with a correct MAC and time is **refused**, and any record of its
pid is removed, when:

- a key or value is malformed, a key is repeated, or a required key is
  missing;
- it has both `argv` and `clear`, or neither;
- the program name is relative (such as `./run.sh`) or starts with `-`;
- `cwd` is not absolute;
- an argument is not valid base64 of UTF-8, or contains a control
  character;
- `need` names a key the server does not implement;
- its pid is not running, runs as another user, or is not the pane's first
  process or one of its descendants.

Accepted reports replace the pane's record. A record is dropped when its
program exits while the server is running (an exit in the last two seconds
before the server stops still counts as running), when its program sends
`clear=1`, or when you close the pane. Records are not removed by a terminal
reset, the alternate screen, clearing the screen or prompt marks.

## Restoring

Records are saved with the rest of the workspace, in a file only you can
read and write. Every program in the pane inherits the token, so a report
only claims a command. The pmux server therefore also records the executable
the reporting process is really running, and checks it again each time it
checks the process is still alive. A wrapper that reports and then runs its
target is bound once the target is running.

When the pmux server restores a pane, it uses the pane's record if all of
these hold:

- saved commands are re-run for this restore (Update and restart always does;
  otherwise `[mux] space_open_runs_commands` decides);
- the reported command is the program its owner was last seen running. An
  absolute program path must be that same file. A bare name must be that
  file's name, and the name must find that same file on the restored
  shell's `PATH`, so another program of the same name earlier on `PATH`
  cannot take its place;
- that program's name is trusted (see below);
- the record was made by the same user;
- the reported directory still exists;
- the last resume of this record started and stayed up for ten seconds. A
  resume that failed, or crashed the server, is not tried again.

Otherwise it falls back to what it restored before: the command it finds by
inspecting the pane's processes. A record it skips is kept, logged with the
reason, and shows the reason as `not_resumed` in the pane snapshot. A record
whose command never matched its program says "reported command does not
match the running program".

To resume, the server types `pmux resume-run ID` into the restored pane's
shell, after the shell has read its startup files. That command fetches the
record from the server over its private socket and replaces itself with the
verified program file, giving it the reported name as `argv[0]`. Each
argument reaches the program unchanged; the shell never parses them. The
program runs as the shell's foreground job, so suspending or quitting it
returns to the shell. The ID works once.

## Trust

Only trusted programs are resumed without asking, and trust applies to the
verified program, not to the name a report claims. By default these are
`nvim`, `vim`, `hx`, `emacs`, `kak`, `micro`, `lazygit` and `tig`: editors and
Git interfaces that reopen files or a repository.

Trusting a program trusts the arguments its reports carry. Several of these
programs run commands given as arguments, such as `nvim -c` or
`emacs --eval`, so trusting them means a report can replay such a command
after a restart. Only a process of your own user in that pane, running that
very program, can make the record. To choose your own list, set it under
`[mux]`, matched on the program's file name:

```toml
[mux]
resume_trusted = ["nvim", "lazygit", "htop"]
# resume_trusted = ["*"]   # trust every command a program reports
```

The list is read when the pmux server starts.

## Root, Windows and other systems

- When the pmux server runs as root, it sets no tokens, takes no reports and
  resumes nothing, unless you set `resume_allow_elevated = true` under
  `[mux]`.
- On Windows the pmux server does not set `OSC7577_TOKEN` and keeps restoring
  from its own process inspection. It cannot yet check a report's owner or
  start a resume without building command text for `cmd` or PowerShell.
- Systems other than Linux, macOS and Windows behave like Windows.

## Limits

- A report can be up to 2048 bytes, including the escape sequence.
- Environment variables cannot be reported; put specific values in the
  command's own options.
- Only the reported command comes back. Shell variables, history, and
  wrappers that did not report (such as `sudo -s`) do not.
