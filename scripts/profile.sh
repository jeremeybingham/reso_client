#!/usr/bin/env bash
# Profile one load-test scenario (#162): a timed run, then a callgrind run of the sync child.
#
#   scripts/profile.sh [SCENARIO] [RECORDS] [-- extra loadtest options]
#   scripts/profile.sh clean 100000
#
# Builds the `profiling` Cargo profile (release + line tables, not stripped). The fake server runs
# natively; only the `reso-ingest sync` child runs under `/usr/bin/time` or valgrind (LOADTEST_WRAP).
# Results go to target/profile/<date>-<commit>-<scenario>-<records>/ (git-ignored):
#   loadtest-time.txt, time.txt   the timed run: loadtest row, child wall s / CPU s / peak RSS
#   loadtest-callgrind.txt        the callgrind run's loadtest output
#   callgrind.out.<pid>           raw profile (open in kcachegrind/qcachegrind if wanted)
#   annotate-self.txt, annotate-inclusive.txt   top functions by self and inclusive cost
# PROFILE_FEATURES="a,b" builds with those reso-ingest features (e.g. parquet); they are added to the folder name.
# PROFILE_NO_DEFAULT=1 adds --no-default-features (system allocator instead of mimalloc); folder name gets -nodefault.
# Load-test output goes to $PROFILE_WORKDIR (default /dev/shm/reso-profile, keeps writes off disk) and is deleted.
set -euo pipefail

scenario=${1:-clean}
records=${2:-100000}
shift $(($# < 2 ? $# : 2))
[[ ${1:-} == -- ]] && shift

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
workdir=${PROFILE_WORKDIR:-/dev/shm/reso-profile}
commit=$(git rev-parse --short HEAD)
dirty=$(git diff --quiet HEAD -- . ':!*.md' || echo "-dirty")
features=${PROFILE_FEATURES:-}
no_default=${PROFILE_NO_DEFAULT:+--no-default-features}
out="target/profile/$(date +%F)-${commit}${dirty}-${scenario}-${records}${features:+-${features//,/-}}${no_default:+-nodefault}"
mkdir -p "$out"
out=$(cd "$out" && pwd)

cargo build --profile profiling -p reso-ingest --example loadtest ${no_default:+"$no_default"} ${features:+--features "$features"}
bin="target/profiling/examples/loadtest"

echo "== timed run: $scenario, $records records -> $out" >&2
LOADTEST_WRAP="/usr/bin/time -a -o $out/time.txt -f wall_s=%e,user_s=%U,sys_s=%S,peak_rss_kib=%M" \
    "$bin" --scenario "$scenario" --records "$records" --workdir "$workdir" "$@" \
    2>&1 | tee "$out/loadtest-time.txt"

echo "== callgrind run (20-100x slower)" >&2
LOADTEST_WRAP="valgrind --tool=callgrind --separate-threads=no --dump-instr=no --callgrind-out-file=$out/callgrind.out.%p" \
    "$bin" --scenario "$scenario" --records "$records" --workdir "$workdir" "$@" \
    2>&1 | tee "$out/loadtest-callgrind.txt"
rm -rf "$workdir"

for f in "$out"/callgrind.out.*; do
    callgrind_annotate "$f" >"$out/annotate-self.txt"
    callgrind_annotate --inclusive=yes "$f" >"$out/annotate-inclusive.txt"
done
echo "== results in $out" >&2
cat "$out/time.txt" >&2
