# ADR-0060 — Interactive TUI cockpit (query runner, point inspector, recent searches, help overlay, theme toggle)

- **Status:** Accepted
- **Date:** 2026-06-23
- **Phase:** 4 (platform & polish)
- **Supersedes / superseded by:** extends [ADR-0036](0036-retro-cockpit-design-system.md) (the retro cockpit design system).

## Context

The retro cockpit (ADR-0036) ships a live dashboard and a constellation view,
both decoupled behind a render-to-buffer API so the same code drives the live
terminal, the unit tests (ratatui `TestBackend`/`Buffer` assertions), and the
committed PNG screenshots (`just tui-shots`). What it lacked was *interaction
beyond browsing*: there was no way to run a query from inside the cockpit,
inspect a result, recall a previous query, discover the keybindings, or change
the look. The roadmap's §3.E ("interactive cockpit") called for exactly these.

Two design constraints carried over from ADR-0036 and must be preserved:

1. **Every screen renders to a `Buffer`** (no terminal needed) so it is unit
   testable and screenshot-able from the *real* render path.
2. **The event/state handler is pure** — key handling must be a plain function
   over `&mut App` that returns a value, with all network I/O pushed to the
   edge, so the whole interaction model is table-testable without a terminal or
   a server.

The live cockpit before this change folded its key handling inline into the
async `run_loop`, mixing state mutation with `await`ed fetches — untestable
without a terminal and a live server.

## Decision

Add a third view, the **query runner**, plus a global **help overlay** and a
live **theme toggle**, and refactor key handling into a pure dispatcher.

### Pure event handling

`App::handle_key(&mut self, key: KeyEvent) -> Effect`. All synchronous state
transitions (navigation, view switches, text-input editing, help toggle, theme
toggle) happen inside `handle_key` and return `Effect::None`. Anything that
needs the network returns an `Effect` describing the work:

```
enum Effect { None, Refresh, EnterConstellation, Requery, RunSearch(String) }
```

`run_loop` awaits the returned effect and calls the matching async method
(`refresh` / `enter_constellation` / `requery_constellation` / `run_search`),
which performs the fetch and then applies the result with a synchronous mutate.
Network code stays a thin shell at the edge; `handle_key` and every apply step
are pure and table-tested.

### The query runner (`View::Search`)

Entered with `/` on the selected collection. One screen carries three of the
requested capabilities:

- **Query runner** — a text input; `Enter` embeds-and-searches the collection
  via `POST /v1/collections/{name}/query/text` (ADR-0047 server-side embedding).
  A non-success response (e.g. *no `[embedding.<collection>]` configured*) is
  surfaced verbatim in the panel rather than dropped.
- **Point inspector** — the selected hit's id, score, and pretty-printed JSON
  payload in a side panel; `↑/↓` move the cursor.
- **Recent searches** — a bounded, de-duplicated, most-recent-first list on
  `App`; `Enter` on an empty input repeats the most recent query (shell-style
  recall). A fuller history selector is deliberately out of scope.

### Help overlay and theme toggle

- `?` (or `F1`) opens a **modal help overlay** listing the keybindings; `Esc` /
  `?` / `F1` dismiss it. While open it swallows other input.
- `Ctrl-t` cycles the **theme** between the warm **Bronze** default and a cool
  **Slate** alternate. A control-modified key is used deliberately so it never
  inserts a character into the query input. The palette is a thread-local in the
  `theme` module read by the semantic style functions, so the ~30 render call
  sites are unchanged and the default thread stays Bronze (the brand consts and
  the screenshot-tool parity are untouched).

### Telemetry

The status panel already drew a points-trend sparkline; it gains a second
**ingest-rate** sparkline (per-poll deltas of the cumulative total), satisfying
the "telemetry sparklines" ask without polling `/metrics` (a heavier path left
to ADR-0054's Prometheus surface).

## Consequences

- The cockpit is now interactive end-to-end, all behind the render-to-buffer +
  pure-handler split, so the new views and the whole keymap are covered by
  `TestBackend`/`Buffer` assertions and table tests — no live terminal in CI.
- The committed screenshots gain `search.png`, `help.png`, and `theme-slate.png`
  (the Slate palette), regenerated from the real render by `just tui-shots` and
  visually verified.
- The query runner depends on a per-collection embedding provider being
  configured server-side; without one it shows the server's error, which is the
  honest behaviour. The live search/refresh fetches are the only untestable
  shells (network), marked as such.
- Theme is process-thread-local state. The cockpit is single-threaded, so this
  is sound; it is not a general multi-tenant theming system and does not try to
  be (`ponytail`: thread-local now, thread a palette param if the renderer ever
  goes multi-threaded).
