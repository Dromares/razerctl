#!/usr/bin/env bash
# One-command local CI for razerctl: the substitute for hosted Actions (billing-dead, DAN-1427).
# A non-author runs this at the PR head SHA and attaches the log as evidence.
#
#   scripts/local-ci.sh [LOG_FILE]        # LOG_FILE defaults to ./local-ci-<sha>.log
#
# Steps: toolchain print, pkg-config libs, build, test, clippy (vs baseline), fmt --check (vs baseline),
# xvfb smoke of razerctl-gui (read-only, isolated HOME). Exit 0 only if every step passes.
# Override CLIPPY_BASELINE / FMT_BASELINE only in the PR that lowers them; never raise them.
set -u
cd "$(dirname "$0")/.." || exit 2

CLIPPY_BASELINE=${CLIPPY_BASELINE:-11}  # warnings on main @ 0988034 (rustc 1.99.0); a new warning fails
FMT_BASELINE=${FMT_BASELINE:-16}        # files `cargo fmt --check` flags on main @ 0988034 (476 hunks); not reformatted

# Agent runs may redirect $HOME; the toolchain lives in the real home.
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
export RUSTUP_HOME=${RUSTUP_HOME:-$real_home/.rustup}
export CARGO_HOME=${CARGO_HOME:-$real_home/.cargo}
export PATH=$CARGO_HOME/bin:$PATH

sha=$(git rev-parse HEAD)
log=${1:-local-ci-${sha:0:7}.log}
exec > >(tee "$log") 2>&1

fails=()
t0=$(date +%s)
step() { echo; echo "=== $*"; }
fail() { fails+=("$1"); echo "!!! FAIL: $1"; }

step "head"
echo "sha=$sha dirty=$(git status --porcelain | wc -l) files"
echo "RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME"
rustc -V || fail "rustc missing"
cargo -V || fail "cargo missing"

step "pkg-config xkbcommon wayland-client gl libudev"
pkg-config --exists xkbcommon wayland-client gl libudev && echo ok || fail "pkg-config libs"

step "cargo build --release --locked"
cargo build --release --locked || fail "build"

step "cargo test --release --locked"
cargo test --release --locked || fail "test"

step "cargo clippy --all-targets (baseline $CLIPPY_BASELINE warnings)"
clippy_out=$(cargo clippy --all-targets 2>&1); clippy_rc=$?
echo "$clippy_out"
# rustc's trailing "N warnings emitted" summary line is not a finding.
n=$(grep '^warning: ' <<<"$clippy_out" | grep -vc 'generated [0-9]* warning\|warnings\? emitted')
echo "clippy: rc=$clippy_rc warnings=$n baseline=$CLIPPY_BASELINE"
if [ "$clippy_rc" -ne 0 ] || [ "$n" -gt "$CLIPPY_BASELINE" ]; then fail "clippy ($n > $CLIPPY_BASELINE)"; fi

step "cargo fmt --check (baseline $FMT_BASELINE files)"
fmt_out=$(cargo fmt --check 2>&1); fmt_rc=$?
files=$(grep '^Diff in ' <<<"$fmt_out" | sed 's/^Diff in \(.*\):[0-9]*:$/\1/' | sort -u | wc -l)
hunks=$(grep -c '^Diff in ' <<<"$fmt_out")
echo "fmt: rc=$fmt_rc files_with_diffs=$files hunks=$hunks baseline=$FMT_BASELINE"
[ "$fmt_rc" -ne 0 ] && echo "(run 'cargo fmt' to see/fix; not auto-applied)"
if [ "$fmt_rc" -ne 0 ] && [ "$files" -gt "$FMT_BASELINE" ]; then fail "fmt ($files > $FMT_BASELINE)"; fi

step "xvfb smoke: razerctl-gui launches and stays up 5 s (read-only, isolated HOME)"
if command -v xvfb-run >/dev/null; then
  tmp=$(mktemp -d)
  HOME=$tmp XDG_CONFIG_HOME=$tmp/cfg XDG_DATA_HOME=$tmp/data RAZERCTL_GUI_DEMO=nodevice \
    xvfb-run -a timeout 5 target/release/razerctl-gui
  rc=$?
  rm -rf "$tmp"
  # 124 = still running when timeout fired = launched fine. 0 = clean early exit. Anything else = crash.
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 0 ]; then echo "smoke ok (rc=$rc)"; else fail "gui smoke (rc=$rc)"; fi
else
  fail "xvfb-run missing"
fi

step "summary ($(( $(date +%s) - t0 )) s wall-clock)"
if [ ${#fails[@]} -eq 0 ]; then echo "RESULT: PASS @ $sha"; exit 0; fi
echo "RESULT: FAIL @ $sha"
printf '  - %s\n' "${fails[@]}"
exit 1
