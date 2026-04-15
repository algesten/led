# Driver: config-file

## Purpose

Loads user configuration files (`keys.toml` and `theme.toml`) from the config directory and parses them into typed `ConfigFile<Keys>` / `ConfigFile<Theme>` domain objects. A single generic `driver<F: TomlFile>()` is instantiated twice — once per file type — sharing a single `ConfigFileOut` command stream but producing separate `Stream<Result<ConfigFile<F>, Alert>>` result streams (see `/Users/martin/dev/led/led/src/lib.rs:315-316`). In principle also a resource driver for persistence and an input driver for hot-reload file-watch; in practice, both of those roles are stubbed or absent today (see § Edge cases).

## Lifecycle

Starts during `led::run` wiring. `config_file_out` is a derived stream from `AppState` (`/Users/martin/dev/led/led/src/derived.rs:177-186`) that fires `ConfigFileOut::ConfigDir { config, read_only }` exactly once per `WorkspaceState` transition that resolves a config directory:

- `Loading → Standalone` → `(startup.config_dir, read_only: true)`
- `Loading → Loaded(w)` → `(w.config, !w.primary)` — primary instance gets write access (on paper), secondary instances read-only.

The stream is `.dedupe()`'d, so in a single run the driver typically reads each file exactly once. Both driver instances (Keys and Theme) subscribe to the same `ConfigFileOut` stream, so every dispatch reads both files in sequence (in whichever order tokio happens to schedule the two spawn_local tasks).

Each driver instance runs two concurrent tasks:

1. A forwarder that observes the outgoing `Stream<ConfigFileOut>` via `.on()` and `try_send`s into an internal mpsc command channel.
2. A `tokio::task::spawn_local` that drains the command channel, dispatches to `read_and_send` on `ConfigDir`, and ignores `Persist`.

There is no explicit shutdown. When the outer streams drop, the `.on()` callback stops firing and the spawn_local tasks exit on channel close.

## Inputs (external → led)

- **Filesystem reads**: `std::fs::read_to_string(<config_dir>/<file_name>)` per `ConfigDir` dispatch. Returns the file contents if readable, or the bundled `default_toml()` (`include_str!`) if the read fails for any reason (missing file, permission denied, etc.). Note: the fallback is blanket — any read error is swallowed into the default, not surfaced as an `Err(Alert)`. Only **parse** errors produce `Err(Alert)`.

- **File watchers**: **none actually present in the driver.** Despite the POST-REWRITE-REVIEW.md § "Hot-reload doesn't actually work" wording suggesting "file watcher infrastructure exists but the round-trip isn't completed", inspection of `/Users/martin/dev/led/crates/config-file/src/lib.rs` shows no notify/watcher setup whatsoever. The driver only re-reads on explicit `ConfigFileOut::ConfigDir` dispatch, which is itself dedup'd and bounded by `WorkspaceState` transitions. [unclear — whether the review's "watcher infrastructure" refers to another crate's shared watcher that could in theory be wired in, or is a stale comment].

## Outputs from led (model → driver)

| Variant                     | What it causes                                    | Async? | Returns via                            |
|-----------------------------|---------------------------------------------------|--------|----------------------------------------|
| `ConfigFileOut::ConfigDir(ConfigDir)` | Read `<config>/<file_name>` from disk, parse TOML | yes    | `Ok(ConfigFile<F>)` or `Err(Alert)` on parse failure |
| `ConfigFileOut::Persist`    | **No-op.** Handler is `ConfigFileOut::Persist => {}` at `crates/config-file/src/lib.rs:57` | — | (nothing) |

**`Persist` is a live dead-letter.** The variant exists, the enum consumer matches it, and the body is an empty block. Nothing in `/Users/martin/dev/led/led/src/derived.rs` or the model emits `Persist` today — searching the codebase finds only the enum definition and the empty match arm. If anything did emit it, the driver would silently discard it.

The `ConfigDir` struct carries a `read_only: bool` flag. This is plumbed from `derived.rs` (primary=false, secondary=true, standalone=true) but the read path in `read_file` never consults it (see `crates/config-file/src/lib.rs:81-92`). Today the flag is informational only — the driver has no write path for it to gate. Rewrite should decide whether to remove the flag or use it meaningfully.

## Inputs to led (driver → model)

Two result streams (one per file type) of shape `Result<ConfigFile<F>, Alert>`:

| Variant                                 | Cause                                                          | Frequency |
|-----------------------------------------|----------------------------------------------------------------|-----------|
| `Ok(ConfigFile<Keys>)`                  | Successful read + parse of `keys.toml` (or fallback default)   | once per `ConfigDir` dispatch; bounded by workspace-state transitions (typically 1-2 per run) |
| `Ok(ConfigFile<Theme>)`                 | Same for `theme.toml`                                          | same      |
| `Err(Alert)` on keys channel            | `toml::from_str` failed to parse `keys.toml` contents          | rare — requires malformed user config |
| `Err(Alert)` on theme channel           | `toml::from_str` failed to parse `theme.toml` contents         | rare      |

Consumed in `/Users/martin/dev/led/led/src/model/mod.rs:367-376`:

```rust
let muts: Stream<Mut> = drivers
    .config_keys_in
    .map(|r| match r {
        Ok(v) => Mut::ConfigKeys(v),
        Err(a) => Mut::alert(a),
    })
    .or(drivers.config_theme_in.map(|r| match r {
        Ok(v) => Mut::ConfigTheme(v),
        Err(a) => Mut::alert(a),
    }));
```

Reducer assigns trivially (`mod.rs:812, 836`):

- `Mut::ConfigKeys(v) => s.config_keys = Some(v);`
- `Mut::ConfigTheme(v) => s.config_theme = Some(v);`

A downstream stream `keymap_s` (`mod.rs:163`) derives the compiled keymap from `s.config_keys` — so after a `ConfigKeys` arrives, the keymap is rebuilt via `Keys::into_keymap()`.

## State owned by this driver

None persistent. Each driver instance holds:
- An mpsc command sender and receiver for the tokio task bridge.
- An mpsc result sender and receiver for the result stream bridge.
- No cached previous-read, no in-flight tracking.

Every `ConfigDir` dispatch performs a fresh blocking `read_to_string` and parse. There is no deduplication inside the driver — the upstream `.dedupe()` in `derived.rs:183` handles that.

## External side effects

- Reads from `<config_dir>/keys.toml` and `<config_dir>/theme.toml`.
- No writes. No filesystem watchers.
- No network.
- No process spawning.

## Known async characteristics

- **Latency**: single synchronous `fs::read_to_string` + `toml::from_str`. Sub-millisecond for realistic config sizes.
- **Ordering**: per-driver-instance, strict (single mpsc + single task). Across the two instances (Keys + Theme), order is undefined — tokio schedules the two spawn_local tasks independently, so a single `ConfigDir` push can produce `ConfigKeys` before `ConfigTheme` or vice versa. The model merges them with `.or()` and the reducer handles each independently, so ordering is irrelevant to correctness.
- **Cancellation**: none. A read is atomic from the driver's view.
- **Backpressure**: command channel capacity is 64 (`mpsc::channel::<ConfigFileOut>(64)`). `try_send` is used in the `.on()` forwarder (`lib.rs:46`), so if the channel is full, the send is dropped silently. In practice the channel is almost always empty because dispatches are extremely rare.
- **Error funnel**: only parse errors surface. Read errors silently become defaults. Missing file is indistinguishable from permission-denied is indistinguishable from corrupt-directory-entry — all three just produce `default_toml()`.

## Translation to query arch

| Current behavior                                          | New classification                                                              |
|-----------------------------------------------------------|---------------------------------------------------------------------------------|
| `ConfigFileOut::ConfigDir` dispatch                       | Resource driver for `Request::LoadConfig(kind, path)`                           |
| Parse result `Ok(ConfigFile<F>)`                          | Resource result → `ConfigState::keys`, `ConfigState::theme` as `Loaded<T>`     |
| Parse result `Err(Alert)`                                 | Alert event or error variant on the same resource slot                          |
| `ConfigFileOut::Persist` (no-op)                          | **Drop** unless hot-reload is explicitly in scope for the rewrite. POST-REWRITE-REVIEW.md calls for an explicit decision. If in scope: `Request::SaveConfig(kind, contents)`. |
| (nonexistent) file watcher                                | Input driver → `Event::ConfigChanged(kind)` via `fs` watcher, dispatching `Request::LoadConfig` in response |
| Dedup via `.dedupe()` in derived                          | Natural in query arch: re-dispatching the same Request with the same args is a no-op at the resource layer, or the domain atom already has a matching `Loaded<T>` and the reducer skips the re-fetch. |

The two-driver-instances-sharing-one-command-stream pattern does not translate cleanly: the query arch would have one resource driver handling `Request::LoadConfig(Kind)` where `Kind` discriminates Keys vs Theme, returning a single `Event::ConfigLoaded(Kind, Result<...>)`. This is simpler than today's generic double-instantiation.

## State domain in new arch

Config lives in a `ConfigState` domain atom (per POST-REWRITE-REVIEW.md § "Fields that currently live in AppState but belong in a domain atom"):

- `ConfigState::keys: Loaded<ConfigFile<Keys>>`
- `ConfigState::theme: Loaded<ConfigFile<Theme>>`
- Possibly `ConfigState::keymap: Derived<Keymap>` computed from `keys` (today's `AppState::keymap` is set by a separate stream `keymap_s`, which is essentially a query over `config_keys`).

Startup flow: `WorkspaceState::Loaded` transition → dispatch `Request::LoadConfig(Keys)` + `Request::LoadConfig(Theme)` → driver reads, parses, emits results → domain atom populated. No `Versioned<T>` needed because config isn't rebased against edits; it's whole-file replace semantics.

## Versioned / position-sensitive data

None. Config files don't have a version concept — each load is a full replacement. No rebase function needed.

## Edge cases and gotchas

- **Hot-reload is wired but non-functional.** Per POST-REWRITE-REVIEW.md, editing `keys.toml` or `theme.toml` at runtime has zero effect. Restart (or a workspace-state transition) is required. The rewrite must make an explicit decision: implement real hot-reload via fs-watch, or drop the `Persist` variant and document that config is startup-only.

- **`ConfigFileOut::Persist` is dead.** Enum variant exists, no emitter in the codebase, handler is an empty match arm. Safe to remove in the rewrite unless hot-reload or write-back is scoped in.

- **No actual file watcher in the driver.** The POST-REWRITE-REVIEW note about "watcher infrastructure" is potentially misleading — the config-file crate itself has no notify setup. Any hot-reload would require wiring the shared `FileWatcher` (used by `workspace` and `docstore`) to register the two config paths and emit `ConfigDir` pushes on change. [unclear — whether the rewrite should fold this into the shared `fs` driver (per DRIVER-INVENTORY-PLAN.md suggestion) or keep it as a distinct config-domain concern].

- **`read_only: bool` on `ConfigDir` is unused by the reader.** The flag is computed correctly in `derived.rs` but `read_file` ignores it. Currently harmless — there's no write path it could gate — but flag-without-effect is a latent bug source. Either drop it or, if `Persist` becomes real, gate writes on it.

- **Error fallback to default on any read failure** is undifferentiated. The user cannot tell whether their `keys.toml` is missing, unreadable, or simply doesn't exist. Consider surfacing permission errors as an Alert in the rewrite.

- **Both driver instances read their own file on every `ConfigDir` push**, even if only one actually changed. Today this is a non-issue because dispatches are rare. In a hot-reload world, fs-watch events would usually be file-specific; the rewrite should route `Event::ConfigChanged(Keys)` only to the keys loader, not both.

- **Bundled defaults live in the driver crate, not in `core`.** `include_str!("default_keys.toml")` at `lib.rs:106` and `include_str!("default_theme.toml")` at `lib.rs:96`. This means swapping the driver implementation (e.g. for tests) must either preserve those defaults or accept different defaults.

- **Parse is strict.** `toml::from_str` with `.as_info()` — any deserialization error produces an info-severity Alert and the config is not updated. State for that slot stays `None` (for Keys, that means no keymap — every keystroke is dropped except in input dialogs). POST-REWRITE-REVIEW.md notes this scenario as a target for a golden: malformed `keys.toml` should produce an alert, not a silently-broken editor.

- **Default `Keys` vs. missing `keys.toml`.** A missing file path silently becomes the bundled default. A present-but-empty file (or an empty `[keys]` table) parses to an empty `Keys` struct — which, after `into_keymap()`, is an empty keymap. This has the same symptom as malformed config but goes through the success branch. The rewrite should consider whether empty-but-valid config should warn.

- **`TomlFile` trait is open.** Any future config file can plug in by implementing `TomlFile`. Today only `Keys` and `Theme` exist, but this is the extension hook if e.g. `settings.toml` lands (for the hardcoded-values items in POST-REWRITE-REVIEW.md § "Hardcoded settings that look like they should be configurable").

## Goldens checklist

- `config-file/keys-loaded-default` — no `keys.toml` present, default bundled config is used, default bindings work.
- `config-file/keys-loaded-custom` — scenario writes a custom `keys.toml` to the scenario's `config_dir` before spawn, verifies a rebinding takes effect (e.g. `"ctrl+q" = "quit"`).
- `config-file/theme-loaded-custom` — same pattern for `theme.toml`, verify a themed color is rendered.
- `config-file/keys-malformed-alerts` — scenario writes `"{{not toml"` to `keys.toml`, verifies an info Alert appears and that default bindings still work [gap — today there is no default-fallback on parse error; the slot stays `None`. Need to re-verify: is default-fallback the desired behavior, or is today's no-keymap-on-parse-error intentional?].
- `config-file/theme-malformed-alerts` — same for `theme.toml`.
- `config-file/hot-reload-noop` — [gap — documents current broken behavior, not a functional test. Replace with a real hot-reload golden once hot-reload is implemented in the rewrite].
- `config-file/persist-no-op` — [gap — nothing to test; `Persist` has no observable effect. Remove this row once the variant is deleted].
- `config-file/secondary-readonly` — [unclear — `read_only` flag has no observable behavior today. Skip until it gates something].
