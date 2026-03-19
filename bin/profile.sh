#!/bin/bash
set -e

cargo build --release

# Run led with pseudo-TTY. It will quit after ~100 iterations.
# stderr goes to a file for timing output.
script -q /dev/null sh -c './target/release/led --flamegraph led/src/model/sync_of.rs 2>/tmp/led_timing.txt'

echo "=== Timing results ==="
cat /tmp/led_timing.txt
