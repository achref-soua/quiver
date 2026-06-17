# ADR-0036: Retro cockpit design system (Bronze Quiver theme, logo, decoration vocabulary)

- **Status:** Proposed
- **Date:** 2026-06-17
- **Deciders:** Achref Soua

## Context

The `quiver-tui` cockpit (ADR-0014, the Phase-3 constellation view) works but is
visually thin: three hard-coded amber tints, a plain text header, a status
paragraph, a collection list, and the braille scatter. Quiver's identity — *a
quiver of vector-arrows, rendered in a retro terminal* — is asserted in the README
("amber phosphor") but not really expressed in the cockpit itself.

`v0.16.0` makes the terminal experience the headline: a strong, coherent retro
personality with a real brand, a memorable logo, and a vocabulary of small,
minimalist retro decorations (tables, graphs, database icons, log lines,
relationship trees, status badges) that help an operator *understand the data* —
what collections exist, how big they are, how they relate, how healthy the server
is, and what is happening — at a glance.

Two foundational choices were the owner's (taken at session start): the **colour
brand** and the **logo**.

## Decision

### 1. Brand palette — "Bronze Quiver"

The cockpit wears the colour of a quiver: aged **bronze** chrome, **leather**
borders, **parchment** body text, an oxidised-copper **verdigris** accent, with
**rust-red** alerts and **moss** for healthy/ready, all on near-black **oak**. One
module (`theme`) is the single source of truth; every widget draws from semantic
roles, never raw colours.

| Role | Name | Hex | Use |
|---|---|---|---|
| `chrome` | bronze | `#CD7F32` | logo, bright borders, headings, primary chrome |
| `text` | parchment | `#E8D8B0` | body text, values |
| `dim` | leather | `#8A5A2B` | labels, faint borders, secondary text |
| `accent` | verdigris | `#3FB6A8` | selection, highlights, the logo arrowhead, links |
| `alert` | rust-red | `#D2552F` | offline, errors |
| `ok` | moss | `#8FB339` | ready, healthy |
| `bg` | oak-black | `#100C08` | background |

Two derived verdigris shades (`accent_hi` `#8EE9DC`, `accent_lo` `#216B61`) give
the logo arrowhead its 3-D bevel. The palette replaces the scattered
`AMBER`/`AMBER_DIM`/`OFFLINE` constants; the README's "amber phosphor" framing is
updated to "bronze".

### 2. Logo — "QUIVER, the V is a 3-D arrowhead"

The wordmark is block-letter **QUIVER** in bronze, where the **V is a 3-D
arrowhead** in verdigris: a downward wedge, **open in the top-middle** (a notch
between two shoulders) so it reads as a *V* and as an arrowtip at once, bevel-shaded
(lit left arm, shadowed right arm) for depth, converging to a sharp point — no
shaft, no fletching (the arrowhead alone). It renders with half-block characters
(`▀▄`) for crisp diagonals. A full banner is the dashboard hero; a compact one-line
variant fits the header. The letter glyphs live in `logo` as bitmaps so the banner
is deterministic and testable.

### 3. Decoration vocabulary

A `decor` module of small, reusable, minimalist retro widgets, each pure enough to
unit-test and each drawing only from the theme:

- **Framed panel** — a titled, bordered region with a retro double/heavy rule and a
  bronze title chip.
- **Database / collection icon** — a small ASCII "drum" (`╭─╮ ╞═╡ ╰─╯`) marking a
  collection, sized to one or three cells.
- **Retro table** — aligned columns with a header rule and selection highlight, for
  the collection browser (name · dim · metric · index · points).
- **Graphs** — a horizontal **bar chart** (`█▉▊…`) for per-collection point counts
  and a **sparkline** (`▁▂▃▄▅▆▇█`) for a metric trend, plus the existing braille
  scatter.
- **Log line** — a timestamped activity line with a severity glyph (`●` info,
  `▲` warn, `✕` error) in the role colour.
- **Relationship tree** — a `├─ └─►`-style tree showing a collection's structure
  (collection → index → metric → encryption), so relationships are visible.
- **Status badge / pill** — a bracketed `[ ONLINE ]` / `◉ ready` chip, and a
  fletched-arrow divider (`»———►`) as a section rule.

### 4. Reproducible screenshots

Cockpit screenshots are generated from the **real render**, not mocked: each screen
is rendered to a ratatui `Buffer` (via the testing backend) with rich demo data and
serialized to **PNG** by a small generator. So the cockpit must expose a
**render-to-buffer API** decoupled from the live HTTP client: the view functions
take plain data structs, and a `render_screen(screen, demo, size) -> Buffer` helper
drives them. Both the tests and the screenshot tool call it.

The generator lives in a **workspace-excluded** crate `tools/cockpit-shots`
(`[workspace] exclude`), so its image/rasterizer dependencies stay out of the
`just verify` gate (lint/test/doc/deny/audit) and the shipped crates entirely. It
depends on `quiver-tui` by path, rasterizes each buffer cell (bg rect + glyph in
the cell's colour) with a monospace font, and writes PNGs to `docs/assets/cockpit/`.
`just tui-shots` runs it. The PNGs are committed and embedded in the README and the
docs site; regenerating them is a single command, so they never go stale. The
interactive asciinema cast remains an owner / real-terminal artifact (never
fabricated, ADR-0014 / roadmap).

## Consequences

- **+** A coherent, memorable retro identity: one brand, a real logo, and a
  decoration vocabulary that makes the data legible at a glance.
- **+** The render-to-buffer refactor makes the whole cockpit **testable** (assert
  cells in a `Buffer`) — coverage rises from "logic only" to the rendered UI.
- **+** Screenshots are deterministic and reproducible (`just tui-shots`), so the
  README/docs visuals match the code and regenerate in one command.
- **+** The screenshot tool's image dependencies are quarantined in an excluded
  crate, so the gate and the shipped binaries gain **no** new dependency.
- **−** A larger `quiver-tui` (theme + logo + decor modules) and a refactor of the
  existing single-file view code; mitigated by module boundaries and tests.
- **−** Committed PNGs add binary weight to the repo (small, and the source of
  truth is the generator).

## Alternatives considered

- **Keep the amber, just add widgets** — rejected: the owner chose a distinct
  "colour of a quiver" brand (Bronze Quiver); amber stays available as a tint but
  is no longer the identity.
- **SVG screenshots (no deps) instead of PNG** — attractive (zero dependencies,
  diffable), but PNG renders identically everywhere and can be visually verified
  during development; the dependency cost is avoided by excluding the generator
  crate from the workspace, so PNG wins without taxing the gate.
- **A TUI framework theme crate** — unnecessary; ratatui `Style`s from a small
  `theme` module are enough and keep control local.
- **Fabricated/mocked screenshots** — never; the screenshots are the real render of
  real demo data, the same standard as the benchmark numbers.

## References

- ADR-0014 (observability / the cockpit), ADR-0035 (docs site — where the
  screenshots are also surfaced), `docs/roadmap.md` (Phase 3 cockpit DoD, the
  asciinema cast as an owner artifact).
