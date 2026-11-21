#!/bin/bash
# Builds the baseline (origin/a) and this tree, then times the cases in
# bench/cases.txt on a 20M-event dataset held in memory, interleaved, best of N.
set -u
ROOT=$(pwd)
export RUSTFLAGS="-C target-cpu=x86-64-v3"
D=${DATA:-d20}; CASES=${CASES:-bench/cases.txt}
mkdir -p /dev/shm/$D /dev/shm/out && cp -r data-cache/$D/. /dev/shm/$D/ && find data-cache/$D -type f -exec dd if={} iflag=nocache count=0 status=none \; ; free -m | head -2
git worktree add -q /tmp/base origin/a 2>/dev/null || true
(cd /tmp/base/native && cargo build --release 2>&1 | tail -1)
(cd "$ROOT/native" && cargo build --release 2>&1 | grep -E "^(warning|error)" -A8; cargo build --release 2>&1 | tail -1)
BASE=/tmp/base/native/target/release/shopping-sessions
NEW=$ROOT/native/target/release/shopping-sessions
# Each bench/v/NAME/ holds source files that replace the baseline's.
for d in bench/v/*/; do
  v=$(basename $d)
  rm -rf /tmp/v_$v && cp -r /tmp/base/native /tmp/v_$v && rm -rf /tmp/v_$v/target && cp $d/* /tmp/v_$v/src/
  (cd /tmp/v_$v && cargo build --release 2>&1 | grep -E "^error" -A8; cargo build --release 2>&1 | tail -1)
done
bench() {
RUNS=${RUNS:-5}
declare -A best line; unset names
names=()
while IFS='|' read -r name which envs; do
  [ -z "$name" ] && continue; case "$name" in \#*) continue;; esac
  names+=("$name"); best[$name]=999999; line[$name]=""
  eval "W_$((${#names[@]}-1))='$which'"; eval "E_$((${#names[@]}-1))='$envs'"
done < "$CASES"
for round in $(seq "$RUNS"); do
  for i in "${!names[@]}"; do
    name=${names[$i]}; eval "which=\$W_$i"; eval "envs=\$E_$i"
    case "$name" in 1:*) [ "$round" -gt 3 ] && continue;; esac
    bin=$NEW; [ "$which" = base ] && bin=$BASE; [ -d /tmp/v_$which ] && bin=/tmp/v_$which/target/release/shopping-sessions
    rm -rf /dev/shm/out/*
    start=$(date +%s%N)
    out=$(env DATA_DIR=/dev/shm/$D OUTPUT_DIR=/dev/shm/out NCPU=4 $envs "$bin" 2>&1)
    real=$(( ($(date +%s%N) - start) / 1000000 ))
    if [ "$real" -lt "${best[$name]}" ]; then best[$name]=$real; line[$name]=$(echo "$out" | tr '\n' ' '); fi
  done
done
echo "=== best of $RUNS, real ms (process start to exit) ==="
for name in "${names[@]}"; do printf '%-14s %6s | %s\n' "$name" "${best[$name]}" "${line[$name]}"; done
}
echo "--- as copied"; grep -E "Shmem|FileHuge|ShmemHuge" /proc/meminfo | tr "\n" " "; bench
