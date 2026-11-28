# Launch choice

Presented frames from a private server on macOS. The window is the real host.
A saved Space owned one live session. The frames are the host's presented
image, not a desktop screenshot.

`choice.png` is the prompt. Restore is selected. The countdown is running.

`restore.png` is Enter. The saved Space, its tab, and the session are back.
The same process was still running.

`deadline.png` is no key. When the countdown ended, the result matched Restore.
The same process was still running.

Esc is not pictured. The same run left an empty window, kept the saved Space
on the rail, cleared that session's Space owner, and left the process running
so it can be attached again. The empty window's shell prints the machine
account, so that frame is not stored here.

The script is `tests/native/start-fresh-choice-e2e.py`. It covers Esc, Enter,
and the deadline. On Linux it starts a private display and sends those keys
to the host window.
