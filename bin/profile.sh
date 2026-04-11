#!/bin/bash
set -e

cargo build --release

# Run led with pseudo-TTY. Replays bin/profile.keys in a loop so a profiler
# (Instruments, samply, cargo flamegraph) can attach to a steady-state workload.
# stderr goes to a file for timing output.
script -q /dev/null sh -c './target/release/led --keys-file bin/profile.keys led/src/model/sync_of.rs 2>/tmp/led_timing.txt'

echo "=== Timing results ==="
cat /tmp/led_timing.txt
