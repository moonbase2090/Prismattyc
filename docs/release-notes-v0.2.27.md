# Prismattyc 0.2.27 release notes

## Pane streams survive interrupted system calls

Prismattyc retries pane reads interrupted by the operating system. Pane
streams no longer drop with the `Interrupted system call` error.
