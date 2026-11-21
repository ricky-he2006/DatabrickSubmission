#!/bin/bash
# Cycle samples per source line of the build named by $REF (default origin/a), 4 threads, 20M events.
set -u
REF=${REF:-origin/a}
sudo apt-get install -y -qq "linux-tools-$(uname -r)" linux-tools-common >/dev/null 2>&1 || sudo apt-get install -y -qq linux-tools-azure >/dev/null 2>&1
sudo sysctl -q -w kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0
PERF=$(ls /usr/lib/linux-tools/*/perf 2>/dev/null | head -1); [ -z "$PERF" ] && PERF=perf
mkdir -p /dev/shm/d20 /dev/shm/out && cp -r data-cache/d20/. /dev/shm/d20/
git worktree add -q /tmp/p "$REF"
cd /tmp/p/native
RUSTFLAGS="-C target-cpu=x86-64-v3" CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_STRIP=false cargo build --release 2>&1 | tail -1
BIN=/tmp/p/native/target/release/shopping-sessions
run() { rm -rf /dev/shm/out/*; env DATA_DIR=/dev/shm/d20 OUTPUT_DIR=/dev/shm/out NCPU=4 "$@"; }
for i in 1 2 3; do run $BIN; done
for i in 1 2; do run /usr/bin/time -f "%e real %U user %S sys %F majflt %R minflt" $BIN; done
for i in 1 2; do run /usr/bin/time -f "1thr: %e real %U user %S sys %R minflt" env NCPU=1 $BIN; done
run $PERF stat -e task-clock,cycles,instructions,page-faults,branches,branch-misses $BIN 2>&1 | grep -vE "^$"
run $PERF stat -d -d $BIN 2>&1 | grep -vE "^$"
run $PERF stat -e dTLB-loads,dTLB-load-misses,dTLB-stores,dTLB-store-misses,L1-dcache-load-misses,LLC-loads,LLC-load-misses $BIN 2>&1 | grep -vE "^$"
run $PERF record -q -e branch-misses -c 2000 -o /tmp/bm.data $BIN
echo "=== branch misses by line"; $PERF report -i /tmp/bm.data --no-children --sort srcline --stdio --percent-limit 1 2>/dev/null | grep -vE "^#|^$" | head -40
run $PERF record -q -e LLC-load-misses -c 2000 -o /tmp/llc.data $BIN 2>/dev/null && { echo "=== LLC misses by line"; $PERF report -i /tmp/llc.data --no-children --sort srcline --stdio --percent-limit 1 2>/dev/null | grep -vE "^#|^$" | head -40; }
EV=cycles; $PERF stat -e cycles true >/dev/null 2>&1 || EV=cpu-clock
run $PERF record -q -e $EV -F 9999 -o /tmp/perf.data $BIN
$PERF report -i /tmp/perf.data --no-children --sort sym --stdio --percent-limit 0.3 2>/dev/null > /tmp/sym.txt
$PERF report -i /tmp/perf.data --no-children --sort srcline --stdio --percent-limit 0.15 2>/dev/null > /tmp/line.txt
echo "=== by symbol"; grep -vE "^#|^$" /tmp/sym.txt | cut -c1-160
echo "=== by source line"; grep -vE "^#|^$" /tmp/line.txt
