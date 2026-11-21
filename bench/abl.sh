#!/bin/bash
# Dedup and sessions ablations on diag4: phase ms by SS_ABL.
set -u
git worktree add -q d4 origin/diag4
(cd d4/native && RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release 2>&1 | tail -1)
B=$PWD/d4/native/target/release/shopping-sessions
cp -r data-cache/d20 /dev/shm/d20
mkdir -p /dev/shm/o/user_daily /dev/shm/o/campaign_daily
run() { # abl threads
  DATA_DIR=/dev/shm/d20 OUTPUT_DIR=/dev/shm/o SS_ABL=$1 NCPU=$2 $B 2>&1 | grep -o '\(parse\|dedup\|sessions\|total\)=[0-9.]*' | tr '\n' ' '
}
for rep in 1 2; do
  for abl in 0 128 256 384 512 1024 2048 3456; do
    echo "rep$rep abl=$abl | 1thr $(run $abl 1) | 4thr $(run $abl 4) | $(run $abl 4)"
  done
done
