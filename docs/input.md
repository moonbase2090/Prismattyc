# Terminal input and text

## Select and copy text

The host owns selection and clipboard controls. These controls do not modify
the terminal grid. Selection uses absolute history rows so that scrolling
does not change the selected text. A new selection replaces the previous one.
A click without a drag does not leave a one-cell selection.

Double-click to select a word. Triple-click to select a row. Copy removes
trailing spaces from each selected row. The host clears a finished selection
when new child output would make it stale.

Use the [configuration reference](config.md) for shortcuts and overrides.
Use the [compatibility matrix](fidelity-matrix-v1.md) for supported behaviors
and their regression tests.

## Open hyperlinks

Move the pointer over an HTTP or HTTPS link to show the hand cursor.
The host recognizes URLs in terminal text and named OSC 8 hyperlinks.
Hold Ctrl and click to open a link on Linux. On macOS, hold Command and click.
Plain clicks keep their normal selection or application behavior.

## Route mouse input

When application mouse tracking is off, the host handles selection. When
tracking is on, the host sends mouse reports to the application. Hold Shift
to select text through the host while application tracking is on.

Tracking modes 1000, 1002, and 1003 control which events are reported.
Mode 1006 selects SGR coordinates. Unsupported mouse encodings are listed
in the compatibility matrix.

## Represent wide text

A width-two character occupies a leading cell and a continuation cell.
The renderer paints the leading cell once. Copy extracts the character once.
Combining marks belong to the preceding character. Supported emoji clusters
include ZWJ, skin-tone, and regional-indicator sequences. This is not a full
implementation of Unicode grapheme segmentation.

The primary screen reflows when its width changes. Alternate-screen grids
retain grid semantics. Reflow preserves authored spaces, cursor placement,
wide characters, and combining marks within the configured history budget.

## Encode keyboard input

Host shortcuts take priority over application keyboard encoding. Applications
can enable supported Kitty CSI-u progressive keyboard flags. The primary
and alternate screens keep separate flag stacks. Each stack holds at most
16 entries. A full terminal reset clears both stacks.

The compatibility matrix lists the supported flags and remaining limits.
