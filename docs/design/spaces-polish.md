# Polish Spaces controls

Status: Implemented and validated for package 0.1.309.

Package 0.1.310 places the top rail above the tab strip. Tab painting,
damage regions, hover targets, and title-row actions use the same offset.
The installed build passes all 36 Spaces acceptance checks, including
separate rail and tab context menus in all four positions. The final host
commit is `1fd0869`. Its 742 host tests, Clippy, and six Termwright scenarios
pass. The preceding workspace run passes all 2,071 tests at `218a292`.
The final receipt is
`build/spaces-acceptance/rail-top-install/verified.json`.

| Change | Behavior |
| --- | --- |
| Save state | Show Saved, Unsaved, Save failed, or Save unavailable. Compare saved tab membership and session layout structure. Ignore terminal output, window size, and focus changes. |
| Optional autosave | Save changed arrangements after two idle seconds. Keep manual saving as the default. Stop automatic retries on failure. |
| Context menu | Group pane layout, session, and Space operations. Explain that closing an attached view keeps membership. Name the target session and Space when confirming a kill. |
| Undo | Retain one window-specific receipt for removal or a Space move. Preserve processes. Refuse stale definitions, changed session identities, and a restarted daemon. Killing cannot be undone. |
| Team details | Use singular counts, omit absent roles, show plain freshness labels, and wrap long descriptions. |
| Crowded rail | Shorten session lists with a hidden count. Keep the focused chip visible. Reserve create and overflow controls. Open a searchable picker from overflow. |
| Startup | Offer ask, restore, and fresh preferences. Restore reconnects live sessions and leaves stopped sessions stopped. |
| Rail position | Expose bottom, left, top, and right in Spaces settings. Place the top rail above the tab strip. Reuse the existing live configuration reload and shared paint/hit geometry. |

## Verify the changes

Run the workspace tests and Clippy. Run `demo/docker/spaces-team.sh` against
the candidate binaries. Inspect the captured rail positions, grouped menu,
save states, Undo result, overflow picker, and restored window.
Run `scripts/termwright-e2e.sh` for terminal regressions. Keep native host
checks separate from Termwright terminal checks.

## Validation results

Code commit: `a0dbcd9d138a7b0eed7949af5d4e01265ee8a2b1`.

- Workspace tests: 2,071 passed, zero failed, across 40 test suites.
- Clippy: passed for the workspace and all targets with warnings denied.
- Termwright terminal regressions: six scenarios passed.
- Spaces acceptance tests: 36 checks passed against the installed binaries.
- Installed binary hashes match the acceptance-test receipt.
- Native screenshots cover all four rail positions, settings, grouped menus,
  kill confirmation, save states, Undo, overflow, and startup restore.

Native visual validation covers Linux X11. It does not establish visual
parity on Wayland, macOS, or Windows.

The local receipts are in `build/spaces-polish-tests-final-head.log`,
`build/spaces-polish-clippy-final-head.log`,
`build/spaces-polish-termwright.log`, and
`build/spaces-team-box/polish-installed/result.json`.
The installation receipt and binary hashes are in
`build/spaces-acceptance/polish-install-20260911T204602Z/verified.json`.

The undo store compares saved definitions under the ownership lock. It
then applies an inverse through the existing recovery journal. Empty
sessions created for individual-pane moves lose their Space ownership
when the pane returns. An expired receipt cannot replace newer changes.
