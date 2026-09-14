# Alternatives analysis — rich layer

**Status:** v1, 2026-08-13. Alternatives-gate input for PRD §5.6.
**Kind:** Versioned survey of substitutes, complements, and transport concerns
for Prism's opt-in, capability-gated rich attachment layer.
**Not:** a rubber stamp. Per §5.6, the gate concludes **pass**, **fail**, or
**inconclusive**; this document supplies the survey and the falsifiable wedge
claim, and it can legitimately conclude that a current approach already meets
the wedge.

Related: [PRD.md](PRD.md) §1.5 (A-2, A-5), §1.6, §5.6;
[capability-protocol.md](capability-protocol.md);
[hybrid-rendering.md](hybrid-rendering.md).

## What is being compared

Prism's bet is a specific combination, not any single feature:

1. **Structured** rich content: typed cell-rect and viewport attachments
   composited over the classic grid, not opaque pixels.
2. **Capability-negotiated**: apps query first; no affirmative reply means
   classic-only. No sniffing, no emit-and-hope.
3. **Fallback-safe by construction**: the classic grid is always present and
   authoritative; rich failure cannot blank or replace it.
4. **Classic fidelity as a release gate**: unaware apps get a high-quality
   plain terminal.

An alternative "meets the wedge" (fail condition, §5.6) if it delivers that
combination today without Prism-specific host work and without a defensible
interoperability gap.

## Kitty graphics protocol

**Solves:** Raster image display in the terminal done seriously: image
transmission (direct, file, shared memory), placements referenced by id,
z-index relative to text, unicode-placeholder positioning, animation frames,
and deletion commands. It has real multi-terminal adoption beyond Kitty itself
(WezTerm, Konsole, Ghostty implement subsets), and it demonstrated that an
APC-based extension can coexist with classic terminals — a precedent Prism's
envelope explicitly builds on (distinct namespace; see capability protocol).

**Falls short of the bet:** It is a *pixel* protocol. The terminal learns
where an image goes, never what it is — no structure, no semantic content, no
interactivity beyond what the app repaints. Detection is by terminal
heuristics or querying with a graphics escape and watching for a response;
there is no general capability negotiation covering versions and feature
subsets. Fallback is the app's problem: an unaware terminal may print garbage
unless the app guesses correctly. Subset divergence across implementing
terminals is real and undocumented per-host.

**Classification:** Substitute for the raster-image slice of rich content;
also a design precedent. Not a substitute for structured/interactive
attachments.

## Sixel and iTerm2 inline images

**Solves:** Sixel is the widest-deployed inline-graphics encoding (xterm,
mlterm, WezTerm, foot, mintty, recent VTE/Windows Terminal work), detectable
via DA1 attribute 4. iTerm2's OSC 1337 `File=` protocol is the simplest
possible "show this PNG here" mechanism and is honored by iTerm2, WezTerm,
and others.

**Falls short of the bet:** Sixel is a 1980s paletted raster format: no
placement model beyond the cursor, no ids, no updates without full repaint,
awkward color depth, and scrolling/reflow behavior that differs by terminal.
OSC 1337 has no negotiation at all — apps typically sniff `TERM_PROGRAM`.
Both are pixels, both push fallback responsibility onto the app, and neither
offers structure or input routing.

**Classification:** Substitutes for static inline images only. Their
transport behavior (Sixel survives some paths OSC/APC do not, and vice versa)
is a useful data point for the capability-transport gate.

## OSC 8 hyperlinks

**Solves:** Semantic markup in the grid itself: a URI attached to a run of
cells, degrading to plain text on unaware terminals with zero risk. Broad
adoption (VTE terminals, iTerm2, WezTerm, Kitty, Windows Terminal) because
the fallback story is perfect.

**Falls short of the bet:** It is one attribute, not a surface. But it is the
strongest existing evidence that *fallback-safe semantic attachment to cells*
is adoptable — exactly Prism's shape, at minimal scale. If discovery shows
authors' pain is mostly "clickable references," OSC 8 plus host features may
meet the wedge; that would be a legitimate gate fail for broader rich work.

**Classification:** Complement (Prism supports it on the classic path) and a
scale-limited substitute for the simplest structured cases.

## Notcurses and TUI frameworks (ratatui, Textual)

**Solves:** Cell-grid-native richness from the *application* side. Notcurses
squeezes remarkable output (planes, sprixels via Sixel/Kitty where available,
media playback) out of existing terminals. ratatui and Textual give authors
widgets, layout, and reactive models; Textual can even serve the same app to
a browser (`textual serve`), which is its own answer to the fallback problem.

**Falls short of the bet:** These are client libraries running on top of
whatever the host terminal offers. They inherit every host limitation: cell
resolution (or per-host graphics quirks via detection heuristics), no typed
attachment model, no negotiated capabilities beyond terminfo and sniffing.
They do not compete with Prism's host-side layer — they are the audience for
it. A Prism client library would sit at exactly this level, and PRD A-2 is
tested by whether authors of such apps adopt a queryable surface.

**Classification:** Complements — and the candidate adopter segment. Also a
substitute in the narrow sense that "good enough at cell resolution" may
dissolve the pain Prism targets; discovery must distinguish these.

## Browser/GPU-backed terminals

**VS Code terminal (xterm.js + extensions):** Solves rich adjacency by
putting the terminal *inside* an application shell: terminal links, image
addon (Sixel/iTerm2), shell integration decorations, and full webview UI one
panel away. Falls short as a *terminal* bet: the richness belongs to the
editor and its extension API, not to a protocol a standalone CLI can target;
outside VS Code the same app is classic-only with no negotiated middle
ground. **Substitute for users already living in VS Code; not a portable
protocol.**

**Warp (blocks UI):** Solves output structure — commands and outputs grouped
into navigable blocks, with host-rendered rich elements — by having the
*host* impose structure via shell integration. Falls short of the bet in the
opposite direction from Kitty: the structure is host-defined and largely
proprietary; apps cannot attach arbitrary negotiated content, and fidelity
questions arise precisely because the host reinterprets the session.
**Substitute for the "terminal sessions deserve structure" thesis; validates
demand, does not provide an app-facing open protocol.**

**WezTerm / Kitty as programmable hosts:** Solve extensibility via host-side
scripting (Lua, kittens) plus their graphics protocols. Falls short because
extensions are per-host programs, not content an application ships; an app
targeting "WezTerm users with this Lua config" has no fallback story
elsewhere. **Complements as implementation precedent; weak substitutes.**

## tmux passthrough and the mux transparency problem

tmux is not a rich-content alternative; it is the transport reality any rich
protocol must survive. By default tmux consumes or drops unknown APC/OSC
sequences; `allow-passthrough` (tmux 3.3+) lets an app wrap payloads in a
DCS `tmux;` envelope with doubled ESCs — but it is off by default, the app
must know it is inside tmux, and the outer terminal's capabilities are
invisible through the mux. Kitty graphics, Sixel, and OSC 8 each behave
differently under tmux, and each protocol's users carry per-mux workaround
lore. This is exactly PRD A-3 and the capability-transport gate: Prism's
query-first design means "no reply through the mux ⇒ classic-only," which is
a safe default rather than a leak. Being *both* the mux and the terminal is
Prism's structural answer, but it must still behave correctly as a middlebox
for foreign protocols and as a client under foreign muxes.

**Classification:** Transport concern, not a substitute. Evidence source for
the transport gate.

## Web apps / localhost servers as the escape hatch

**Solves:** Today's dominant workaround: when a CLI needs real richness, it
prints a `localhost` URL (or uses OSC 8) and opens a browser — dashboards,
profilers, notebook servers, `textual serve`. The browser offers unmatched
layout, input, accessibility, and zero terminal-compat burden.

**Falls short of the bet:** The cost is context rupture: a second surface
with separate lifecycle, state, and auth; broken over plain SSH without port
forwarding; the terminal-native workflow (pipes, scrollback, mux panes,
copy/paste) is abandoned at the boundary. This workaround's *recurrence and
cost* is precisely what the A-1/A-5 discovery gates must document. If authors
show no material cost to the browser hop, the wedge fails honestly.

**Classification:** The strongest substitute. Prism's claim is not "better
than a browser" but "cheaper than the hop for small structured surfaces."

## Wedge

**Falsifiable claim:** No current approach lets a terminal application ship
*one* artifact that (a) negotiates a versioned capability before emitting any
rich content, (b) attaches structured cell-anchored or viewport content over
an unmodified classic grid, and (c) degrades to a fully useful classic UI on
every non-Prism host and through non-passthrough muxes, with no sniffing and
no payload leakage. The claim is falsified if an eligible author demonstrates
that combination today with existing protocols and no Prism-specific host
work — e.g., Kitty-graphics-plus-heuristics with documented safe fallback
across the §5.6 transport matrix, or evidence that OSC 8 / cell-grid TUIs
already cover the material cases found in discovery.

| Alternative | Substitute | Complement | Transport concern |
|---|---|---|---|
| Kitty graphics protocol | Partial (raster slice) | Yes (envelope precedent) | Yes (per-host subsets) |
| Sixel / iTerm2 images | Partial (static images) | No | Yes (differing mux survival) |
| OSC 8 hyperlinks | Partial (simplest cases) | Yes (classic-path feature) | Minor |
| Notcurses / ratatui / Textual | Partial (cell-res sufficiency) | Yes (adopter segment) | No |
| VS Code terminal | Yes (inside VS Code) | No | No |
| Warp blocks | Yes (host-structured thesis) | No | No |
| WezTerm/Kitty scripting | Weak | Yes (precedent) | No |
| tmux passthrough | No | No | Yes (central A-3 risk) |
| Browser / localhost hop | Yes (strongest) | No | No |

## Gate disposition

This v1 records the survey and the wedge claim. It does **not** by itself
record a pass: §5.6 requires cited evidence that the wedge remains unmet,
which depends on discovery (A-1) and transport (A-3) results still in
progress. Disposition as of v1: **inconclusive — survey complete, evidence
links pending**. Update this document with evidence citations and a revised
disposition when the discovery and transport gates report.
