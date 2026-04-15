# Driver: ui

## Purpose

The renderer. It consumes the whole `AppState` stream and produces frames on `stdout` via ratatui over a `CrosstermBackend`. Unlike every other driver, `ui` is output-first — its `driver()` function takes a `Stream<Rc<AppState>>` as its only argument and returns a single `Stream<UiIn>` carrying one rare back-channel event (memory-pressure buffer eviction). All rendering decisions are pure functions of `AppState`: the driver splits state into seven derived sub-streams (display lines, cursor, status bar, tabs, layout, browser/side panel, overlay), deduplicates each, and fans them back together to compose a single `terminal.draw(frame)` call per change. Also ensures `terminal.autoresize()` never performs a real ioctl by caching the backend size on a custom `CachedSizeBackend` wrapper — size changes are pushed from the model via `AppState::dims`, not pulled from the OS per frame.

## Lifecycle

Starts during `led::run` setup (invoked from `derived.rs` wiring — the `ui_in` stream is plumbed into `DriverInputs` at `/Users/martin/dev/led/led/src/lib.rs:38`). `setup()` inside `crates/ui/src/lib.rs:231-240` creates the ratatui `Terminal`, querying the backend once for the initial size. Thereafter, every state emission that changes any of the seven derived projections triggers a draw.

There is no explicit shutdown. The render closure lives as long as the `state` stream it subscribed to. Terminal teardown (leave alternate screen etc.) is owned by `terminal-in`'s `InputGuard`, not by `ui`. This coupling is why the rewrite's renderer-as-query plan needs to be clear about who owns the terminal lifecycle.

Initialization quirk: the `Terminal` is created before any state has arrived, so the first render happens as soon as the first `AppState` propagates through the reactive graph. The `CachedSizeBackend`'s initial `cached_size` comes from an `inner.size()` call at construction — after that, the cache is only refreshed via `set_size()` calls from inside the render closure (using `layout.dims`). If state emits before `AppState::dims` is populated, `layout_inputs` returns `None` and no draw happens; the first frame waits for `Resize` from `terminal-in`.

## Inputs (external → led)

No direct external inputs. The driver is entirely reactive to `AppState`. The only "external" dependency is the terminal backend handle (`io::stdout()`) used for the actual bytes of each frame.

Indirectly, the tab-overflow computation depends on `AppState::dims`, which was populated by `terminal-in::Resize`. So real SIGWINCH eventually affects what this driver emits as `EvictOneBuffer`, but only via AppState.

## Outputs from led (model → driver)

There is no `*Out` enum for ui. Instead, the input is the full `Stream<Rc<AppState>>`. The driver reads whatever it needs off `AppState` via `display::*_inputs()` projection functions (defined in `crates/ui/src/display.rs`). Each projection returns only the fields that affect that panel, which — via `.dedupe()` — is how the driver avoids unnecessary redraws.

| "Variant"                    | What it causes                                           | Async? | Returns via       |
|------------------------------|----------------------------------------------------------|--------|-------------------|
| `AppState` emission          | Recompute seven projections; if any changed, redraw      | no     | (side effect only) |
| `AppState::dims` change      | Re-layout + push new cached size into `CachedSizeBackend` | no     | (side effect)     |
| `AppState::force_redraw` bump | Clear the terminal before next draw (no partial-update) | no     | (side effect)     |

The entire rendering is idempotent and pure over `AppState`. No buffered commands, no request/response protocol, no per-command ack.

## Inputs to led (driver → model)

One variant only. This is the unusual back-channel.

| Variant                | Cause                                                                                                 | Frequency                                   |
|------------------------|-------------------------------------------------------------------------------------------------------|---------------------------------------------|
| `UiIn::EvictOneBuffer` | Tab bar overflowed the editor width **and** at least one non-active, clean, materialized, non-preview buffer exists | per state emission while the overflow condition holds |

Defined at `/Users/martin/dev/led/crates/ui/src/lib.rs:19-21`. Emitted by `overflow_s` at `crates/ui/src/lib.rs:191-203`:

```rust
let overflow_s = state
    .filter(|s| tabs_overflow(s))
    .filter(|s| {
        s.tabs.iter().any(|tab| {
            !tab.is_preview()
                && Some(tab.path()) != s.active_tab.as_ref()
                && s.buffers
                    .get(tab.path())
                    .is_some_and(|b| b.is_materialized() && !b.is_dirty())
        })
    })
    .map(|_| UiIn::EvictOneBuffer)
    .stream();
```

Consumed in `led/src/model/mod.rs:634-640`: `UiIn::EvictOneBuffer → Mut::EvictOneBuffer → action::evict_one_buffer`. The reducer picks the LRU victim by `b.last_used()` and removes the tab (and, via post-fold dematerialization, the buffer itself). See `/Users/martin/dev/led/led/src/model/action/preview.rs:144-170`.

Why this back-channel exists: tab-overflow is cheapest to detect as a function of the rendered tab bar (gutter + label widths summed until they exceed `editor_width`). That computation lives naturally where rendering happens. The alternative — running the same width-sum inside the model to pre-empt overflow — duplicates layout math. So the renderer detects and signals, the model decides the victim.

Subtle property: **this signal fires repeatedly, not once.** Every `AppState` emission that still overflows re-fires `EvictOneBuffer`. Only when the reducer has removed enough tabs to fit, or when no clean non-active buffer remains, does the stream stop firing. This makes it a feedback loop: renderer says "too many," model drops one, renderer recomputes, possibly says "still too many," model drops another. Terminates because each eviction strictly reduces the tab count. The `filter` guards against a degenerate loop where overflow persists but nothing evictable remains — in that case no signal is emitted and the user just sees a clipped tab bar.

## State owned by this driver

Local and minimal:

- `Terminal<CachedSizeBackend>` — the ratatui terminal instance, wrapping `CrosstermBackend<Stdout>` and a `Cell<Size>` cache.
- `last_redraw: RedrawSeq` — a counter captured in the render closure. On each draw it compares against `layout.force_redraw`; if they differ, `terminal.clear()` runs before `terminal.draw()`. This is how the model requests a full repaint (e.g. after an overlay closes and leaves artifacts) without the ui driver needing to know what triggered it.

Notably, none of this is in `AppState`. The design consciously keeps ratatui / backend handles out of state — `AppState` only contains pure domain types (see `feedback_no_driver_types_in_appstate.md`). The renderer re-derives every drawable from state each frame; no persistent per-buffer render cache, no span recycling.

## External side effects

- Writes ANSI sequences to `io::stdout()` for every frame (via ratatui).
- Calls `terminal.clear()` on `force_redraw` mismatch.
- No filesystem, no network, no signals.

## Known async characteristics

- **Latency**: synchronous per state emission. Draw time is microseconds to a few ms depending on viewport size. The `CachedSizeBackend` trick exists specifically to eliminate the per-frame `ioctl(TIOCGWINSZ)` that autoresize() would otherwise do on macOS (historically >1ms).
- **Ordering**: strict — renders happen in state-emission order.
- **Cancellation**: none. If state emits faster than draws complete, the `.on()` callback will serialize them.
- **Backpressure**: none. The reactive tree is single-threaded and push-based — there's no queue between state and render. If rendering is slow, every other subscriber to `state` is blocked behind it. In practice not a problem; draws are sub-millisecond for typical terminal sizes.
- **Dedup**: seven `.dedupe()` calls on the seven projections ensure redraws only happen on visible changes. A cursor-only move doesn't rebuild display lines; a syntax-highlight update doesn't rebuild the status bar.

## Translation to query arch

The renderer absorbs into the runtime loop. Instead of a driver emitting frames as a side effect of `.on()`, the runtime evaluates `render(&state) -> Frame` as a pure query and hands the result to `terminal.draw()`. This matches how modern CycleJS-style and Elm-style architectures treat rendering.

| Current behavior                                     | New classification                                                             |
|------------------------------------------------------|--------------------------------------------------------------------------------|
| 7 derived projections × `.dedupe()` → `combine!` → draw | Single memoized render query over the state domain atoms; ratatui handles diffing |
| `force_redraw: RedrawSeq` in `AppState`              | Dropped. The query always returns the full frame; ratatui/clear logic moves into the runtime. POST-REWRITE-REVIEW.md § "Fields that belong in a domain atom" marks `force_redraw` as "drop". |
| `CachedSizeBackend`                                  | Stays — it's a ratatui integration detail, not a domain concern                |
| Tab overflow → `UiIn::EvictOneBuffer`                | Open question [unclear]. Three options: (a) eviction becomes a pure query that returns a `Request::EvictBuffer` when overflow is detected; (b) the renderer retains a narrow event-back-channel for this one signal; (c) width-sum moves into model as a derived `UiState::tabs_overflow: bool`, and a reducer listens for the transition. (c) duplicates layout math currently owned by render; (a) is cleanest but requires the runtime to route renderer-computed requests, which no other resource driver needs. |

## State domain in new arch

- No input events beyond the optional `EvictOneBuffer` variant above.
- Reads from every domain atom — this is the confluence point.
- No resource results land here.

## Versioned / position-sensitive data

None at the driver level. But the renderer is the point where versioned data becomes visible to the user: LSP diagnostics, syntax highlights, git line status, inlay hints all flow through `AppState` into `display_inputs()` and are drawn against the current buffer. Consequence: **the rewrite's rendering query must respect version tags** — if `AppState` exposes a diagnostic stamped to version N while the buffer is at N+1, the render query must either decline to draw them or rebase them. Today this is handled upstream by freezing diagnostics during edit bursts (`feedback_pull_diagnostics.md`), so the renderer just trusts what's in state.

## Edge cases and gotchas

- **`EvictOneBuffer` fires continuously while overflow persists.** Each state emission re-triggers it. Consumers must be idempotent — removing a tab that doesn't exist is a no-op in `evict_one_buffer`, so the loop is safe. Rewrite must preserve this convergence property.
- **The "evictable" filter is critical.** Without the second `.filter` (non-preview, non-active, materialized, clean), the loop would fire forever on e.g. a single overflowing dirty buffer. Dirty buffers are never auto-evicted — the user must save or explicitly close.
- **`CachedSizeBackend` must be kept in sync.** The render closure does `terminal.backend_mut().set_size(...)` before every `draw()`. Skipping this (or desyncing cached vs real size) breaks ratatui's per-frame autoresize logic. Rewrite should keep this invariant even if the backend wrapper changes shape.
- **`force_redraw` is a `RedrawSeq` — a monotonic counter, not a bool.** The comparison `layout.force_redraw != last_redraw` detects any change in either direction. Don't simplify to `if force_redraw { ... }` because that loses the "one bump = one clear" semantics.
- **The file-search / find-file cursor computation lives in `driver()` not in a `display::*` function.** `crates/ui/src/lib.rs:36-90`. This is a small violation of "cursor is just another display projection" — it's a direct 55-line closure in the driver body. Rewrite's render query should factor this into a named helper to match the other `display::*_inputs` pattern.
- **No double-buffer / no damage tracking.** ratatui does cell-level diffing internally, but the led side rebuilds every `Line` from scratch on every redraw. Large files with many highlighted spans hit this on every cursor move because display lines are rebuilt whenever `DisplayInputs` changes (it includes cursor position). The `dedupe` on `display_s` is only keyed on whole-input equality, which changes on cursor moves even if the *visible* lines wouldn't. [unclear — whether this is a real perf issue or negligible; a profiling golden could verify].
- **`UiIn::EvictOneBuffer` carries no payload.** The model alone decides the victim. This is intentional (LRU is state-side data), but means the renderer cannot veto a specific buffer. Rewrite should preserve the model-picks-victim contract.
- **The driver never reads `AppState::buffers` for rendering** beyond the overflow check. All rendered buffer content flows via `display_inputs()` (which pulls just the active buffer). This makes `dedupe` cheap: the vast majority of state changes (other buffers being mutated, LSP responses for non-active buffers) don't force a redraw.

## Goldens checklist

- `ui/render-initial-frame` — first draw after startup; verifies initial size probe + first render path.
- `ui/cursor-move-only` — move cursor within a line, verify only cursor position changes in the rendered frame (no line rebuild artifacts).
- `ui/resize-shrinks-viewport` — PTY resize (blocked today — needs ioctl support) causes dims update and redraw.
- `ui/force-redraw-clears` — trigger a `force_redraw` bump (e.g. overlay close path), verify `terminal.clear()` runs before the draw.
- `ui/tab-overflow-evicts` — open enough tabs to overflow the tab bar while one is clean and inactive; verify `UiIn::EvictOneBuffer` fires and the oldest clean tab is closed.
- `ui/tab-overflow-no-evictable` — overflow with only dirty / active tabs; verify the signal does **not** fire and the tab bar clips silently.
- `ui/tab-overflow-cascade` — overflow requiring multiple evictions to resolve; verify the feedback loop converges in one scenario run.
- `ui/overlay-completion-clears-on-close` — overlay rendered then dismissed; verify no ghost cells remain (dedup on overlay projection).
- `ui/file-search-cursor-position` — file-search dialog open, cursor reported at panel-relative coordinates.
- `ui/find-file-cursor-position` — find-file dialog open, cursor reported at status-bar absolute coordinates.
- `ui/cursor-hidden-when-not-main-focus` — focus on side panel; verify cursor is None.
