#!/bin/bash
# Decode ablations on diag4: parse ms by SS_ABL, one and four threads.
set -u
git worktree add -q d4 origin/diag4
(cd d4/native && RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release 2>&1 | tail -1)
B=$PWD/d4/native/target/release/shopping-sessions
cp -r data-cache/d20 /dev/shm/d20
mkdir -p /dev/shm/o/user_daily /dev/shm/o/campaign_daily
run() { # mode abl threads
  DATA_DIR=/dev/shm/d20 OUTPUT_DIR=/dev/shm/o SS_MODE=$1 SS_ABL=$2 NCPU=$3 $B 2>&1 | grep -o 'parse=[0-9.]*\|total=[0-9.]*' | tr '\n' ' '
}
for rep in 1 2; do
  for case in "2 0" "3 0" "3 1" "3 2" "3 4" "3 8" "3 16" "3 32" "3 64" "3 31"; do
    set -- $case
    echo "rep$rep mode=$1 abl=$2 | 1thr $(run $1 $2 1) | 4thr $(run $1 $2 4) $(run $1 $2 4)"
  done
  for abl in 0 64; do
    echo "rep$rep full abl=$abl | 4thr $(run 0 $abl 4) | $(run 0 $abl 4) | $(run 0 $abl 4)"
  done
done
