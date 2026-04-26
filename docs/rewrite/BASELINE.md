# Goldens baseline

**Date:** 2026-04-20 (after Mα — harness integration; M1–M5 ship state)

The ~257 scenarios in `goldens/scenarios/` target the full legacy
`led` behaviour. This file records the baseline against the rewrite
so each future milestone can measure its contribution.

## Baseline (post-Mα)

```
smoke:          0 / 5    failing
actions:        0 / 57   failing
keybindings:    0 / 111  failing
driver_events:  0 / 27   failing
config_keys:    0 / 7    failing
edge:           0 / 29   failing
features:      0 / 21   failing
────────────────────────────────
total:          0 / 257
```

**Every failure is a frame mismatch.** The rewrite binary renders a
minimal frame (tab bar row + body, no chrome) while legacy's baseline
frame has:

- A side panel on the left (file browser + gutter with `~` empty
  markers).
- A status bar at the bottom with filename + `L<row>:C<col>`.
- A bracketed `│` separator between side panel and body.

Without those, no frame can match — even `smoke/open_empty_file`
fails trivially.

## How to read the number

The meaningful metric is "what % green are we on scenarios relevant
to the milestone we just shipped." The absolute number stays near
zero until **M9 (UI chrome)** and **M11 (file browser)** land, which
together will turn many visually-simple scenarios green at once (any
scenario whose `dispatched.snap` is already correct).

## How to re-run the baseline

```
cd goldens
for f in smoke actions keybindings driver_events config_keys edge features; do
  cargo test --test "$f" 2>&1 | grep -E "^test result:" | \
    awk -v name="$f" '{print name": "$0}'
done
```

Update this file whenever a milestone flips the pass rate.
