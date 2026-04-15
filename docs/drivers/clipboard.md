# Driver: clipboard

## Purpose

The clipboard driver bridges led's `kill_ring` to the host system
clipboard so that cut/copy (`C-w`, `M-w`, `C-k`) writes propagate to
other apps and paste (`C-y`) reads from the system clipboard with a
fall-back to the kill ring when the system clipboard is empty. It is
a resource driver in the narrow sense — `ClipboardOut::Read` produces a
`ClipboardIn::Text(...)` response — but `ClipboardOut::Write` is
fire-and-forget. A headless variant keeps an in-memory buffer for
tests so that goldens run without touching the real clipboard.

## Lifecycle

One driver instance per process, constructed at startup. The system
variant (`driver`) tries to obtain a real `arboard::Clipboard` handle
once at construction; if that fails (headless env, no display server)
the `Option<arboard::Clipboard>` is permanently `None` and all
subsequent commands silently no-op. The headless variant
(`driver_headless`) always succeeds — it owns a `Mutex<String>` as its
backing store.

No async task. The driver is a synchronous callback (`out.on(...)`)
that runs on whatever thread pushes to the output stream. Shutdown
is implicit: when the `Stream<ClipboardOut>` is dropped, the callback
registration goes with it.

## Inputs (external → led)

- The system clipboard, via `arboard`. Whatever other applications
  wrote to the clipboard is what `Read` returns.
- Nothing else.

## Outputs from led (model → driver)

| Variant                      | What it causes                                                                                 | Async? | Returns via                       |
|------------------------------|------------------------------------------------------------------------------------------------|--------|-----------------------------------|
| `ClipboardOut::Write(text)`  | System variant: `arboard::Clipboard::set_text(text)`; headless: `*guard = text.clone()`        | no (synchronous) | (none — fire-and-forget)          |
| `ClipboardOut::Read`         | System variant: `arboard::Clipboard::get_text().unwrap_or_default()`; headless: clone the buf | no (synchronous) | `ClipboardIn::Text(text)`         |

Both are invoked inline from the `out.on` callback — no channel, no
`spawn`. Latency is whatever `arboard` takes (platform-dependent; see
below).

## Inputs to led (driver → model)

| Variant                      | Cause                                                                                          | Frequency                       |
|------------------------------|------------------------------------------------------------------------------------------------|---------------------------------|
| `ClipboardIn::Text(text)`    | Response to `ClipboardOut::Read`. `text` is the system clipboard's current text, or `""` if the clipboard is empty / non-text / access failed | Per yank action (`C-y`)         |

Consumed in `led/src/model/mod.rs:447-477`: the `clipboard_s` chain
falls back to `s.kill_ring.content` when the received `text` is empty,
then yanks at the active buffer's cursor. No separate "clipboard is
empty" event — an empty string is the signal.

## State owned by this driver

- System variant: `Mutex<Option<arboard::Clipboard>>`. The `Option`
  captures whether `arboard::Clipboard::new()` succeeded. If `None`,
  every command no-ops (including `Read`, which means the model
  waiting for a `ClipboardIn::Text` will hang — in practice this doesn't
  happen because `arboard::Clipboard::new()` succeeds whenever a user
  is running led interactively with a display).
- Headless variant: `Mutex<String>` — the in-memory backing store.
- The `Mutex` is defensive; `out.on` runs on the thread that pushes to
  `clipboard_out`, which in current led is the single main thread. No
  contention expected.

## External side effects

- On macOS: calls into the system `NSPasteboard`.
- On Linux/X11: talks to the X server via x11-clipboard (arboard's
  backend).
- On Linux/Wayland: talks to the compositor via wl-clipboard-rs
  (arboard spawns `wl-copy` / `wl-paste` helpers under some
  configurations — platform-dependent).
- Windows: `OpenClipboard` / `GetClipboardData` / `SetClipboardData`.
- No files written, no network.

## Known async characteristics

- **Latency**: synchronous and usually <1 ms on macOS and Windows. On
  X11, `get_text` can stall for tens to hundreds of ms when the owning
  app is unresponsive. On Wayland, `wl-paste` spawn can add ~10 ms
  per read. `[unclear — not benchmarked in led's test suite]`.
- **Ordering**: commands are processed in the order the outer stream
  emits them. Since the handler is synchronous, ordering is strict.
- **Cancellation**: none.
- **Backpressure**: none — the `out.on` callback drains each
  emission synchronously. No channel, no queue depth.

## Translation to query arch

| Current behavior                                  | New classification                                                        |
|---------------------------------------------------|---------------------------------------------------------------------------|
| `ClipboardOut::Write(text)`                       | `Request::ClipboardWrite(text)` — fire-and-forget (no response event)     |
| `ClipboardOut::Read`                              | `Request::ClipboardRead` → `Event::ClipboardRead(text)`                   |
| `ClipboardIn::Text(text)`                         | `Event::ClipboardRead(text)`                                              |
| Headless variant for tests                        | Same split — a `FakeClipboardDriver` behind the same request protocol     |
| Kill-ring-fallback when `text.is_empty()`         | Stays model-side (combinator / reducer that handles `Event::ClipboardRead` checks the kill ring if `text` is empty) |

The fire-and-forget nature of Write means the rewrite should keep
`Request::ClipboardWrite` without a response event — producing a no-op
event would just add noise. `Read`, by contrast, always produces an
event, even for empty clipboards (`Event::ClipboardRead(String::new())`).

## State domain in new arch

- Nothing persistent lands in any atom. `Event::ClipboardRead` is
  consumed transiently by the yank saga, which then updates
  `BufferState` (cursor, rope contents) and `EditState::kill_ring`.
- The kill-ring lives in `EditState` (per the AppState translation
  table in the inventory plan).
- The "system clipboard contents" is not state led owns — it's just a
  transient value that produces a state transition.

## Versioned / position-sensitive data

None. Clipboard text has no position context; the yank logic in the
model attaches it to the buffer's current cursor at apply time. No
rebase needed.

## Edge cases and gotchas

- **`arboard::Clipboard::new()` can fail at startup** on headless
  systems or in very locked-down environments. The driver captures
  the failure once; there is no retry. Consequence: if the clipboard
  becomes available later (e.g. an X server starts), led won't
  notice until restart.
- **Empty string is a real signal.** `Read` → `ClipboardIn::Text("")`
  is the fall-back trigger for kill-ring-paste in the model. If the
  rewrite ever maps "clipboard access failed" to "empty string" *and*
  "genuinely empty clipboard" to "empty string," both paths land in
  the same fallback — which is what current led does and is fine.
- **The system variant and headless variant have *distinct code
  paths*.** They are two separate `pub fn`s; the runner picks
  `driver_headless` for tests (see `led/src/main.rs` wiring). Keep
  both in the rewrite — goldens run headless, production runs system.
- **Platform backend behavior differs for what counts as "text."**
  X11 treats the primary selection and the clipboard as separate;
  arboard targets the clipboard only. macOS and Windows have a single
  clipboard. A user who copies via mouse highlight on Linux won't see
  that text via `Read`. This is a pre-existing limitation, not a bug
  to fix in the rewrite.
- **`set_text` failures are silently swallowed** (`let _ = cb.set_text(text)`).
  Writing to the system clipboard can fail (e.g. X11 selection-owner
  race); there is no event back to the model. The user sees the kill
  ring update regardless (led's internal copy), so the UX is
  "clipboard didn't update, but internal yank did."
- **Wayland's `wl-copy` backend can spawn a child process that
  survives the led process.** The helper holds the selection on
  Wayland; this is normal and preserved by arboard. The rewrite need
  not worry about cleanup — standard Wayland clipboard semantics.
- **Writes flow from `kill_ring.content.clone()` via a
  `dedupe`.** `derived.rs:390-396` only writes when the kill-ring
  content actually changed. Preserve this: otherwise every keystroke
  in a region that's already selected would issue a redundant
  `set_text` call (expensive on X11).
- **Reads are triggered by bumping `kill_ring.pending_yank.version()`,
  not by the `C-y` action directly.** The action updates the
  versioned field, the derived stream emits `Read`, the driver
  responds, the model yanks. This indirection means the rewrite must
  preserve "user action triggers a clipboard read" in its handler
  chain even if `pending_yank` disappears as a `Versioned<T>`.

## Goldens checklist

Scenarios under `tests/golden/drivers/clipboard/`:

1. `text/` — exists. Write then read round-trip in the headless
   variant. Assert `ClipboardIn::Text` returns the written content.
2. `empty_fallback/` — `C-y` with an empty clipboard; assert the
   kill-ring content is yanked instead (covers the fallback branch
   in `clipboard_s`).
3. `write_dedupe/` — set the same kill-ring content twice; assert
   only one `ClipboardWrite` appears in the trace (validates the
   `.dedupe()` in derived).
4. `unicode_roundtrip/` — write a multi-byte string (emoji, CJK),
   read it back; assert byte-perfect round-trip.
5. `large_text/` — a several-MB clipboard; ensure the headless path
   doesn't truncate and latency is acceptable.
6. `multi_line_yank/` — kill a region spanning three lines; assert
   the yank at a new cursor position places the three lines
   correctly (this is mostly model-side but validates the
   driver-produced text includes newlines).
7. `[future] system_variant_probe/` — hard to golden; arboard is
   non-deterministic across platforms. Document as a manual QA
   scenario rather than an automated golden.
