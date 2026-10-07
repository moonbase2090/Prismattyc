# Prismattyc 0.2.30 release notes

## Cleanly close exited Space sessions

When an attached session exits successfully, Prismattyc now closes its pane,
removes the session from its saved Space, and tears down the live session. A
failed attached client still remains available as a placeholder so it can be
reopened.
