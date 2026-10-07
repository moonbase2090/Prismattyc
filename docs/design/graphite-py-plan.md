# graphite-py plan

**Status:** for MB2090 sign-off. This document is the plan. It adds no runtime code.

**Approver:** MB2090. **Reviewer:** muse-73-74. This pull request stays open until MB2090 signs off. The author does not merge it.

**Where this file lives.** It is in Prismattyc, next to the Graphite crate plan (PR #182, `docs/design/graphite-crate-plan.md`), because that is the review surface for this sign-off. The Graphite repo does not exist yet. When G0 of #182 creates public `github.com/moonbase2090/graphite`, copy this plan there and leave this file as the pointer. The token JSON itself does not live in Prismattyc.

**Baseline.** Prismattyc `origin/main` is `f9c2d73`. `crates/prismattyc-host/src/graphite.rs` is unchanged from `2a4dd0f` (the #182 baseline). Token values in this plan come from that file. `palette.rs` is the command-palette state machine (query, recents, keymap chords). It is not a color palette, and this plan does not port it.

**Consumers.** The Rust `graphite-core` crate is the token source. Rookrunner's terminal dashboard is the first app. Rookrunner today is the `execution-core` package (`pyproject.toml`: `requires-python >= 3.11`, runtime dependency `pyyaml==6.0.3` only). `python -m execution_core dashboard` prints a one-shot text snapshot. `dashboard-html` writes that same text into a file. `get` and `cancel` are plain CLI commands.

## 1. Goals

1. `graphite-core` emits one versioned JSON file of the Graphite tokens, the four bar presets, and the design-pixel spacing. CI rejects a file that disagrees with the Rust constants.
2. `graphite-py` loads that file and draws the Graphite look with Textual: Rail, Pane header, Chip, Badge, StatusDot, TabStrip, and a shortcut overlay. Same models and tokens. Cell snapshots, not Prismattyc pixels. Truecolor and a 16-color profile.
3. Rookrunner's dashboard can use that look as an optional extra. The core install stays on pyyaml. `get` and `cancel` keep working as plain text without the extra. The existing one-shot `dashboard` and `dashboard-html` commands stay.
4. Every dashboard action has a key binding and a mouse target. A click does the same work as the key. The layout stays compact: one row for the rail, one row for a pane header, no tall empty banners.

## 2. Non-goals

- Painting Prismattyc's pixel chrome, IBM Plex, or the light-cycle stamp.
- Porting `palette.rs`, the space picker, or Prismattyc's command list. The overlay takes chord labels the app already resolved.
- Waiting on `graphite-tui` or on Prismattyc host PRs P1–P8. The export needs the core constants, not the host switch.
- Replacing `dashboard` or `dashboard-html`.
- A PyPI release, a new GitHub repo, or any cloud stack. This sequence has no infrastructure. A later publish pipeline, if it needs cloud resources, uses CDK or Pulumi.
- Products other than Rookrunner and the Graphite crate.

## 3. Decisions that differ from the sketch

**The JSON does not ship before the crate split.** Generating it from today's `graphite.rs` would make a third copy, and #182 then moves the constants into `graphite-core`. The export PR runs after that crate has the constants.

**It lands after G1 and G3, not only after G1 and G2.** G1 is tokens and bar presets. G2 is theme derivation for non-brief Prismattyc themes, which this dashboard does not need. G3 is where the design sizes are exercised (`tabs` 44, side rail 220, and the rest). The export PR depends on G0 (public repo), G1, and G3. It can merge before any Prismattyc host PR.

**`palette.rs` stays in Prismattyc.** Rookrunner's overlay lists Rookrunner actions (refresh, inspect, cancel, focus, quit). It does not embed Prismattyc's action enum.

**The pretty view is a new command.** `dashboard` remains the text snapshot that `tests/test_dashboard_options.py` already locks. The Textual app is `dashboard-tui`, present only when the extra is installed.

**Package extra name.** The current project name is `execution-core`, so the extra in this tree is `execution-core[dashboard]`. When the package is renamed to `rookrunner`, the extra stays `dashboard` (`rookrunner[dashboard]`). This plan does not rename the package.

## 4. Repo, license, and dependencies

| Piece | Choice |
| --- | --- |
| Token JSON | `github.com/moonbase2090/graphite`, path `export/graphite-tokens.v1.json`, written by GX1 |
| Python package | Own repo `github.com/moonbase2090/graphite-py`, distribution name `graphite-py`, import `graphite_py` |
| License | MPL-2.0, same as Prismattyc and the Graphite crate. Textual stays a dependency under its own MIT license. No Textual source is copied. |
| Language | Python `>=3.11`, matching Rookrunner |
| Textual | `textual>=8.2.8,<9` |
| Snapshots | `pytest-textual-snapshot` (MIT) as a dev dependency. It provides `snap_compare` and writes SVG. |
| Lint | `ruff` on the same major Rookrunner already pins (`0.12`), `ruff check` and `ruff format --check` |
| Tests | `pytest`. A red test commit comes before the implementation on every code PR. No skipped tests and no silenced lints. |

Textual 8.2.8 was current on PyPI on 2026-10-06 (MIT, `requires-python >=3.9,<4`). The upper bound stops a major bump from moving snapshots in silence. Raising the floor is its own PR with new snapshots in the Proof section.

PyPI on 2026-10-06: `graphite-py` returned 404 (free), `graphite` returned 200 (taken), `moonbase-graphite` and `graphite-tui` returned 404. The publish PR checks again and does not register a name by itself. Nothing in this sequence uploads to PyPI. Until then, Rookrunner pins `graphite-py` by git revision.

`graphite-core` the library does not gain a serde dependency. #182 keeps that edge closed. GX1's writer is an xtask whose `serde_json` dependency is a dev-dependency of the xtask, not of the library.

## 5. Token JSON

### 5.1 Schema

`schema_version` is `1`. The loader rejects any other version, any missing required field, and any unknown field.

```json
{
  "schema_version": 1,
  "source_revision": "git sha of graphite-core",
  "tokens": {
    "dark": { "ground": "101216", "bar": "15181d", "text": "e6e9ee" },
    "light": { "ground": "e9ecf0", "bar": "eef0f3", "text": "1f2329" }
  },
  "bar_presets": {
    "graphite": {
      "dark": { "tabs": "15181d", "status": "0d0f12" },
      "light": { "tabs": "eef0f3", "status": "e4e7ec" }
    },
    "harbor": {
      "dark": { "tabs": "152131", "status": "0e1722" },
      "light": { "tabs": "e3edf8", "status": "d6e3f2" }
    },
    "moss": {
      "dark": { "tabs": "17221b", "status": "0f1712" },
      "light": { "tabs": "e4f0e7", "status": "d7e7db" }
    },
    "plum": {
      "dark": { "tabs": "211a27", "status": "17121c" },
      "light": { "tabs": "f4ece0", "status": "ebe0cf" }
    }
  },
  "spacing_px": {
    "tabs_bar_h": 44,
    "sidebar_w": 256,
    "pane_header_h": 28,
    "rail_h": 30,
    "side_rail_w": 220,
    "window_pad": 8,
    "pane_gap": 8,
    "pane_pad": 12,
    "pane_radius": 8,
    "chip_h": 30,
    "chip_radius": 6
  }
}
```

The `tokens` objects carry every field of the Rust `Tokens` struct (the snippet above is the shape, not the full list). Colors are 6-digit sRGB hex, lowercase, no `#`. `plum` on a light ground is the Sand pair from the design brief. Spacing is design pixels at scale 1, the same numbers `ChromeGeom::px` scales in the host.

### 5.2 Versioning

The file name carries the major schema: `graphite-tokens.v1.json`. A breaking change adds `graphite-tokens.v2.json` and leaves v1 in the tree until `graphite-py` and Rookrunner have both moved. Adding a field is a breaking change under this loader, because unknown fields fail. That is deliberate: a silent new field would let the two sides disagree about what is required.

### 5.3 Drift

1. GX1 generates the JSON from the Rust constants and fails CI if the committed file differs.
2. A Rust test asserts anchor values against the file: dark `ground` is `101216`, dark `bar` is `15181d`, light `bar` is `eef0f3`, light plum tabs are `f4ece0`.
3. `graphite-py` vendors a copy and records `source_revision` plus the sha256. Its CI clones the public Graphite repo at that revision with no token and fails if the sha256 differs. A failed clone fails the job.
4. The Python loader test repeats the same anchors, so a hand-edited file cannot pass on one side only.

16-color is not a second hand-written palette. `graphite-py` maps each sRGB color to the nearest of the 16 ANSI colors, and a golden test locks that map for the brief tokens. Truecolor uses the hex unchanged.

### 5.4 Package layout

```text
graphite-py/
  pyproject.toml
  LICENSE
  src/graphite_py/
    tokens.py            # load and validate JSON
    color.py             # truecolor and 16-color
    theme.py             # Textual theme variables from tokens
    actions.py           # id, key, mouse target
    widgets/
      status_dot.py
      badge.py
      chip.py
      pane_header.py
      tab_strip.py
      rail.py
      shortcut_overlay.py
    demo.py              # python -m graphite_py.demo <widget>
  tests/
  fixtures/graphite-tokens.v1.json
```

One widget per module. The loader does not import Textual. Widgets do not read the filesystem; they take a loaded token object. No module owns both loading and layout.

## 6. Widgets and the input contract

Snapshots use `snap_compare` at 80×24 and at a narrow width. Each widget is snapshotted in truecolor and in the 16-color profile. States, where the widget has them: idle, hover, active, working, unseen, attention, empty, and overflow. A snapshot update is explicit in the PR (`--snapshot-update` is not left on).

| Widget | Looks like | Acceptance |
| --- | --- | --- |
| StatusDot | One cell. Attention, working, unseen, idle. | Snapshot per state. The widget is one cell tall. |
| Badge | The "needs you" mark, and a mail count. | Snapshot for empty (draws nothing), count 1, and a wide count that ellipsizes. |
| Chip | One line: dot, label, optional close. | Click and the key both report the same action id. Overflow ellipsizes the label. |
| Pane header | One row: dot, name, meta, status. | Snapshot height is one row at 80 columns. Hover and focus are separate snapshots. |
| TabStrip | Chips, a trailing add, overflow drops the command field before it shortens labels. | Same decision as #182 G4, measured in cells with a fake width of one column per character. |
| Rail | One row of space or run chips. | Snapshot height is one row. A chip that does not fit is omitted, and a test names which one. |
| Shortcut overlay | Resolved chord, then caption. A run that does not fit is omitted. A notice string draws no keycaps. | Snapshots for fit, drop, and notice. |

**Actions.** `actions.py` is a list of frozen records: `id`, `key`, `mouse` (the widget id a click hits). The dashboard registry is Rookrunner's, built with the same record type. A test fails if any record has an empty key or an empty mouse target. Pilot tests press the key and click the target and require the same callback.

Compact is a test, not a review comment: pane header and rail snapshots are one row, and the empty dashboard is one status line plus the list, with no title block taller than a row.

## 7. PR sequence

One open PR per repo. Each code PR starts with a failing test commit. Feature PRs (everything after the scaffold) include a Proof section: the demo command and the SVG or terminal capture from that command.

### 7.1 Graphite repo

| PR | Depends on | Tests | Acceptance |
| --- | --- | --- | --- |
| GX1 Token JSON | #182 G0, G1, and G3 | Generator output equals `export/graphite-tokens.v1.json`. Anchor colors. `graphite-core` `Cargo.toml` has no `serde`. | Public file. `schema_version` 1. Spacing table matches the design sizes in `graphite.rs`. |

### 7.2 graphite-py

| PR | Depends on | Tests | Acceptance |
| --- | --- | --- | --- |
| Y0 Scaffold | None | `ruff` and `pytest` run in CI on 3.11. | MPL-2.0. No runtime dependency yet. Repo is public. |
| Y1 Loader | GX1, Y0 | Rejects a wrong version, a missing field, and an unknown field. Vendored sha256 matches the pinned Graphite revision. Anchors match §5.3. | `python -m graphite_py.demo tokens` prints the dark ground hex. Proof attaches that output. |
| Y2 Theme and 16-color | Y1 | Truecolor hex round-trips. 16-color golden for the brief tokens. Theme variables expose every token name. | Runtime dep `textual>=8.2.8,<9`. |
| Y3 Dot, badge, chip | Y2 | `snap_compare` for the states in §6. Chip click and key share an action id. | Demo: `python -m graphite_py.demo chip`. Proof attaches the SVG. |
| Y4 Pane header | Y3 | One-row snapshot. Hover and focus differ. | Demo: `python -m graphite_py.demo pane-header`. |
| Y5 Tab strip | Y2 | Overflow drops the command field before labels shorten. Attention and unseen snapshots. | Demo: `python -m graphite_py.demo tab-strip`. |
| Y6 Rail | Y2 | One-row snapshot. Omitted chip is named by the test. | Demo: `python -m graphite_py.demo rail`. |
| Y7 Shortcut overlay | Y2 | Fit, drop, and notice snapshots. | Chords are arguments. Demo: `python -m graphite_py.demo overlay`. |
| Y8 Action contract | Y3 | A record with no key or no mouse target fails the test. | The helper is what Rookrunner K4 calls. |

Y4 through Y7 stay one at a time after Y2. Y5, Y6, and Y7 do not wait on each other in design, and they still merge in that order so review stays on one widget.

### 7.3 Rookrunner

| PR | Depends on | Tests | Acceptance |
| --- | --- | --- | --- |
| K1 Optional extra | Y2 | Default `pyproject.toml` dependencies are still only `pyyaml`. A test imports `execution_core` and fails if `textual` or `graphite_py` imports. | Extra name `dashboard`. Default CI does not install it. |
| K2 `dashboard-tui` command | K1 | `tests/test_dashboard_options.py` stays green. Without the extra, `dashboard-tui` exits non-zero and names the extra. `get` and `cancel` stay green without the extra. | `dashboard` and `dashboard-html` are unchanged. |
| K3 Run list | K2, Y6 | Pilot: the list shows `run_id` and `state` from the same socket calls `render_terminal` uses. Key and click both move the selection. | Proof: `python -m execution_core --state "$PWD/.execution-state" dashboard-tui` plus the SVG. The list is one row per run. |
| K4 Inspect and cancel | K3, Y8 | Key and click each call the same functions as CLI `get` and `cancel`. Those CLI tests still pass with the extra uninstalled. | No action in the dashboard registry lacks a key or a mouse target. |

Order: Y0 can open beside GX1. Y1 needs both. Then Y2. K1 starts once Y2 exists. K3 needs Y6. K4 needs Y8. Rookrunner's default CI never installs the extra. A separate job installs `.[dashboard]` and runs the pilot tests.

## 8. Risks

| Risk | What keeps it small |
| --- | --- |
| The JSON is generated from the host before the split and then moved again | GX1 waits for G1 and G3 in the public Graphite repo. |
| `graphite-core` grows a serde dependency | The writer is an xtask dev-dependency. CI reads the library `Cargo.toml`. |
| Textual's SVG output changes inside 8.x | Floor and `<9` ceiling. A floor bump is a PR with new snapshots in Proof. |
| `dashboard` text tests break | `dashboard-tui` is a separate command. The old tests stay on the default install. |
| The extra leaks into the core environment | K1's import test runs in the default CI. |
| A widget accepts only the keyboard | Y8 and K4 fail a record with no mouse target. Pilot tests click. |
| Snapshots become the only spec | Each widget also has a behavior assertion (height, which chip was dropped, which action fired). |
| PyPI `graphite-py` is taken before publish | The name check is repeated in the publish PR. This plan does not register it. |
| Rookrunner's license is still undecided | `graphite-py` is MPL-2.0. An open-source app can depend on it. A later Rookrunner license choice does not redraw the widgets. |
| Public CI cannot fetch a private Graphite repo | #182 makes that repo public at G0. Y1's pin fetch uses no token. |

## 9. Open questions

| # | Options | Recommendation |
| --- | --- | --- |
| A. Where do the files live? | A1. JSON and Python both in the Graphite repo. A2. JSON in the Graphite repo, package in `graphite-py`. A3. Both in Prismattyc until G0. | **A2.** One generator next to the Rust constants, and a Python toolchain that does not enter the Rust workspace. This sign-off doc stays in Prismattyc only until G0. |
| B. When does the JSON start? | B1. Now, from `graphite.rs`, before `graphite-core` exists. B2. After G0, G1, and G3. B3. After Prismattyc has switched to the core (P8). | **B2.** B1 forks the constants. B3 waits on host paint moves this dashboard does not need. |
| C. How is the JSON written? | C1. `serde_json` in an xtask, dev-dependency only. C2. Hand-rolled writer in the xtask. C3. `serde` feature on `graphite-core`. | **C1.** The library dependency edge stays free of serde, which is the #182 rule. C2 is the fallback if the xtask is too small to justify the crate. |
| D. Textual pin | D1. `textual>=8.2.8,<9`. D2. Exact `8.2.8`. D3. Unpinned. | **D1.** Exact pins stall security fixes. No ceiling lets a major rewrite rewrite every SVG. |
| E. License of `graphite-py` | E1. MPL-2.0. E2. MIT, matching Textual. E3. Wait until Rookrunner picks a license. | **E1.** Same as the token source. Textual remains MIT beside it. E3 blocks the package on an unrelated decision. |
| F. PyPI name | F1. `graphite-py`. F2. `moonbase-graphite`. F3. `graphite-textual`. | **F1.** It was free on 2026-10-06. `graphite` is taken. Recheck at the publish PR. Do not register it now. |
| G. Rookrunner command | G1. New `dashboard-tui`. G2. Replace `dashboard`. G3. `dashboard --tui`. | **G1.** G2 breaks `tests/test_dashboard_options.py`. G3 overloads a command that today prints and exits. |
| H. 16-color map | H1. Derived in Python, locked by a golden test. H2. A second map generated by Rust and stored in the JSON. | **H1.** The JSON stays the sRGB source. If a swatch is wrong, a later schema version can carry an override. |
| I. Schema change policy | I1. New file `v2`, unknown fields are errors, v1 stays until consumers move. I2. A semver field, unknown fields ignored. | **I1.** Ignoring unknown fields is how the two sides drift. |

## 10. What sign-off authorizes

Signing off authorizes GX1, then Y0–Y8, then K1–K4, under the dependencies in §7. It does not authorize a PyPI upload, a package rename, a change to `dashboard` or `dashboard-html`, or a merge of this pull request. MB2090 merges this document, if at all, as a separate step.
