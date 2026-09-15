# Post-release reliability checks

Version 0.2.2 follows the 0.2.1 performance work. It does not change the
published 0.2.0 assets or reset the CRAP baseline.

## Correct keyboard and collection behavior

The native host now sends the xterm sequences for F6 through F12. Before
this change, F6 sent `CSI 16 ~`; it now sends `CSI 17 ~`. F11 and F12 now
send `CSI 23 ~` and `CSI 24 ~`. Named keys and physical-key fallback share
one encoder. Both paths failed the corrected expectations before the fix.

Collection patches reject revision overflow. A patch with base `u64::MAX`
and next revision zero previously wrapped in release builds. Encoding,
decoding, and patch application now use checked addition. A release-mode
negative control reproduced the invalid acceptance.

Command-palette help now describes Messages and Update/Restart correctly.
The move-pane help includes blank terminals.

## Regression coverage

The follow-up checks these user and protocol contracts:

- Mailbox renames preserve queued letters and update prior addresses.
- Restored pane logs retain the newest frames and continue their sequence.
- Pane resizing changes the nearest split and rejects impossible geometry.
- Templates preserve launch recipes and report conflicts before creation.
- Team text distinguishes active, snoozed, and stale requests.
- Collection replies require the current generation and negotiated features.
- Image conversion preserves color and alpha. Clipping preserves nearby pixels.
- Styled snapshot replay preserves terminal text, colors, and attributes.
- Restart options require explicit selection before stopping sessions.
- Update receipts preserve the installation path and reject overwritten metadata.
- Native context menus distinguish removal from killing a session.
- Rail resizing saves the selected width.

## Validation status

Focused contract tests and strict workspace Clippy pass. Full workspace
coverage and native-window checks are pending. The release CRAP target stays
at 123 functions above 40, compared with 168 in the 0.2.1 capture. A new
complete capture must establish the final count before this follow-up is
ready to merge. Mutation results must retain the complete changed-code
universe and use the existing 60% threshold.
