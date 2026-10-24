#!/usr/bin/env bash
# spilman-payer-e2e.sh — reproducible proof that the payer driver
#
#   1. funds a channel at a REAL mint over HTTP (not an in-process one),
#   2. emits the marker contract the PRTA router lane greps for, exactly once,
#   3. leaves ZERO mint calls in the open → settle window (measured through the
#      lane's own counting proxy, not inferred from a log),
#   4. keeps --payee as an alias, and rejects an unknown flag with exit 2.
#
# WHY THIS SCRIPT EXISTS
# ----------------------
# The lane runs on a router over a serial console; reproducing a failure there
# costs a flash cycle. Everything here is the same shape on the host: a real mint
# daemon on a private port, a counting proxy in front of it, the module with the
# Spilman rail enabled, and the driver invoked with the lane's exact flags.
#
# WHAT IT KILLS
# -------------
# Only PIDs it started, tracked in $RUN_DIR/*.pid. No `pkill -f`, no pattern
# matching: another context window's mint or module must not die because of a
# pattern this script found convenient.
#
# USAGE
#   scripts/spilman-payer-e2e.sh                 # build, run, and prove
#   scripts/spilman-payer-e2e.sh --no-build      # use the binaries as they are
#   MINT_PORT=3499 LANE_PORT=2319 PROXY_PORT=3341 scripts/spilman-payer-e2e.sh
#
# EXIT: 0 = every assertion passed; 1 = at least one failed; 3 = a precondition
# was missing (nothing was proven either way — never reported as success).

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKSPACE="${WORKSPACE:-$HOME}"

# Private ports. NOT 3338/3438 (a resident mint may own those) and NOT 2121/2122
# (reserved for other context windows by the lane's own rule).
MINT_PORT="${MINT_PORT:-3499}"
LANE_PORT="${LANE_PORT:-2319}"
PROXY_PORT="${PROXY_PORT:-3341}"

MINT_DAEMON="${MINT_DAEMON:-$HOME/repos/cashu_spilman_channels/target/release/cdk-spilman-test-mintd}"
MODULE_BIN="${MODULE_BIN:-$REPO/target/debug/tollgate-module-basic-rust}"
PAYER_BIN="${PAYER_BIN:-$REPO/target/debug/examples/spilman-payer}"
# The lane's own counting proxy. Reused rather than re-implemented: the point is
# to measure what the lane measures.
PROXY_PY="${PROXY_PY:-$HOME/worktrees/prta-spilman/spilman/mint_proxy.py}"
# Artifact fallbacks: `target/` in this worktree is deleted by other context
# windows' cleanups, so a copy outside it is preferred when present.
ARTIFACT_DIR="${ARTIFACT_DIR:-$WORKSPACE/artifacts-spilman}"

BUILD=1
for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unexpected argument: $arg" >&2; exit 2 ;;
    esac
done

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/spilman-payer-e2e.XXXXXX")"
STARTED_PIDS=()
FAILURES=0
CHECKS=0

say()  { printf '\n=== %s\n' "$*"; }
ok()   { CHECKS=$((CHECKS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { CHECKS=$((CHECKS+1)); FAILURES=$((FAILURES+1)); printf '  FAIL  %s\n' "$*"; }

# Kill only what this script started, newest first, and prove each one is gone.
cleanup() {
    local pid
    for pid in "${STARTED_PIDS[@]:-}"; do
        [ -n "$pid" ] || continue
        kill -TERM "$pid" 2>/dev/null || continue
        for _ in $(seq 1 30); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null
    done
}
trap cleanup EXIT INT TERM

wait_http() {  # wait_http <url> <seconds> [expect-any]
    local url="$1" seconds="${2:-20}" i
    for i in $(seq 1 $((seconds * 5))); do
        if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    return 1
}

# ── preconditions ───────────────────────────────────────────────────────────
say "preconditions"
for tool in curl python3 ss; do
    command -v "$tool" >/dev/null || { echo "MISSING: $tool" >&2; exit 3; }
done
[ -f "$PROXY_PY" ] || { echo "MISSING: the lane's counting proxy at $PROXY_PY" >&2; exit 3; }
if [ ! -x "$MINT_DAEMON" ]; then
    echo "MISSING: the test mint daemon at $MINT_DAEMON" >&2
    echo "  build it: (cd $HOME/repos/cashu_spilman_channels && cargo build --release -p cdk-spilman-test-mint)" >&2
    exit 3
fi

# Ports must be free: binding over someone else's service would prove nothing and
# could break their run.
for port in "$MINT_PORT" "$LANE_PORT" "$PROXY_PORT"; do
    if ss -ltn 2>/dev/null | grep -qE "[:.]$port[[:space:]]"; then
        echo "PORT $port is already in use; pick another with MINT_PORT/LANE_PORT/PROXY_PORT" >&2
        exit 3
    fi
done
ok "ports $MINT_PORT (mint) / $PROXY_PORT (proxy) / $LANE_PORT (module) are free"

# ── build ───────────────────────────────────────────────────────────────────
if [ "$BUILD" = 1 ]; then
    say "build (debug: the proof is protocol behaviour, not codegen)"
    ( cd "$REPO" \
        && cargo build --features payer-driver \
        && cargo build --example spilman-payer --features payer-driver ) \
        || { echo "BUILD FAILED" >&2; exit 3; }
elif [ ! -x "$MODULE_BIN" ] || [ ! -x "$PAYER_BIN" ]; then
    echo "MISSING: --no-build was given but $MODULE_BIN / $PAYER_BIN is not executable" >&2
    exit 3
fi

# Copy the artifacts OUT of target/ first: other context windows' cleanups delete
# it, and a proof that dies because its binary vanished is a proof of nothing.
mkdir -p "$ARTIFACT_DIR"
cp -f "$MODULE_BIN" "$PAYER_BIN" "$ARTIFACT_DIR/" 2>/dev/null || true
MODULE_RUN="$ARTIFACT_DIR/$(basename "$MODULE_BIN")"
PAYER_RUN="$ARTIFACT_DIR/$(basename "$PAYER_BIN")"
[ -x "$MODULE_RUN" ] || MODULE_RUN="$MODULE_BIN"
[ -x "$PAYER_RUN" ] || PAYER_RUN="$PAYER_BIN"
ok "binaries: $(basename "$MODULE_RUN"), $(basename "$PAYER_RUN")"

# ── 1. real mint, on a private port ─────────────────────────────────────────
say "1. start a real mint on 127.0.0.1:$MINT_PORT"
"$MINT_DAEMON" --listen-host 127.0.0.1 --listen-port "$MINT_PORT" \
    > "$RUN_DIR/mint.log" 2>&1 &
MINT_PID=$!
STARTED_PIDS+=("$MINT_PID")
echo "$MINT_PID" > "$RUN_DIR/mint.pid"
MINT_URL="http://127.0.0.1:$MINT_PORT"

if wait_http "$MINT_URL/v1/info" 25; then
    ok "GET $MINT_URL/v1/info answers"
else
    bad "the mint never answered $MINT_URL/v1/info (log: $RUN_DIR/mint.log)"
    echo "--- mint.log ---"; tail -20 "$RUN_DIR/mint.log"; exit 1
fi

# ── 2. counting proxy in front of it ────────────────────────────────────────
say "2. arm the lane's counting proxy on 127.0.0.1:$PROXY_PORT"
python3 "$PROXY_PY" --upstream "$MINT_URL" --bind-host 127.0.0.1 \
    --bind-port "$PROXY_PORT" > "$RUN_DIR/proxy.log" 2>&1 &
PROXY_PID=$!
STARTED_PIDS+=("$PROXY_PID")
echo "$PROXY_PID" > "$RUN_DIR/proxy.pid"
PROXIED_MINT="http://127.0.0.1:$PROXY_PORT"

proxy_ctl() { curl -fsS --max-time 3 "$PROXIED_MINT/__lane/$1"; }

if wait_http "$PROXIED_MINT/__lane/count" 20; then
    ok "the proxy answers its control path"
else
    bad "the proxy never came up (log: $RUN_DIR/proxy.log)"
    echo "--- proxy.log ---"; tail -20 "$RUN_DIR/proxy.log"; exit 1
fi
proxy_ctl reset >/dev/null && proxy_ctl disarm >/dev/null

# The driver must fund AT the proxy: that is what makes assertion F measurable.
# Prove the proxy really is a mint before believing any count.
if curl -fsS --max-time 5 "$PROXIED_MINT/v1/info" >/dev/null; then
    ok "the proxied mint URL serves /v1/info (the driver's --mint-url)"
else
    bad "the proxied mint URL does not serve /v1/info"
fi

# ── 3. the module, with the Spilman rail enabled ────────────────────────────
say "3. start the module on 127.0.0.1:$LANE_PORT with the rail enabled"
CONFIG_DIR="$RUN_DIR/config"
mkdir -p "$CONFIG_DIR"
cat > "$CONFIG_DIR/config.json" <<JSON
{"spilman":{"enabled":true,"mint_url":"$PROXIED_MINT","channel_db_path":"$RUN_DIR/spilman.db"}}
JSON

TOLLGATE_BIND="127.0.0.1:$LANE_PORT" \
TOLLGATE_STATE_DB="$RUN_DIR/tollgate.db" \
TOLLGATE_TEST_CONFIG_DIR="$CONFIG_DIR" \
    "$MODULE_RUN" > "$RUN_DIR/mod.log" 2>&1 &
MOD_PID=$!
STARTED_PIDS+=("$MOD_PID")
echo "$MOD_PID" > "$RUN_DIR/mod.pid"
BASE="http://127.0.0.1:$LANE_PORT"

if wait_http "$BASE/spilman/params" 30; then
    ok "GET $BASE/spilman/params answers"
else
    bad "the module never answered /spilman/params (log: $RUN_DIR/mod.log)"
    echo "--- mod.log ---"; tail -30 "$RUN_DIR/mod.log"; exit 1
fi
ADVERTISED="$(curl -fsS --max-time 5 "$BASE/spilman/params" | python3 -c \
    'import json,sys; print(json.load(sys.stdin).get("mint_url") or "")' 2>/dev/null)"
if [ "$ADVERTISED" = "$PROXIED_MINT" ]; then
    ok "the payee advertises the proxied mint ($ADVERTISED)"
else
    bad "the payee advertises ${ADVERTISED:-<none>}, not $PROXIED_MINT"
fi

# ── 4. the lane's CLI contract, before spending anything ────────────────────
say "4. the driver's CLI contract"
HELP_OUT="$("$PAYER_RUN" --help 2>&1)"; HELP_RC=$?
if [ "$HELP_RC" -eq 0 ] && printf '%s' "$HELP_OUT" | grep -q -- '--mint-url' \
        && printf '%s' "$HELP_OUT" | grep -q -- '--module-url' \
        && printf '%s' "$HELP_OUT" | grep -q -- '--flow' \
        && printf '%s' "$HELP_OUT" | grep -q -- '--mint-file'; then
    ok "--help exits 0 and names --mint-url/--mint-file/--module-url/--flow"
else
    bad "--help exited $HELP_RC or omitted a flag: $HELP_OUT"
fi

"$PAYER_RUN" --payee "$BASE" --mint-url "$PROXIED_MINT" --bogus-flag >/dev/null 2>&1
RC=$?
[ "$RC" -eq 2 ] && ok "an unknown flag exits 2 (got $RC)" || bad "unknown flag exited $RC, want 2"

printf 'http://127.0.0.1:%s\n' "$MINT_PORT" > "$RUN_DIR/mint.txt"
: > "$RUN_DIR/empty.txt"
OUT="$("$PAYER_RUN" --mint-file "$RUN_DIR/empty.txt" --module-url "$BASE" 2>&1)"; RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$OUT" | grep -qi 'empty'; then
    ok "an EMPTY --mint-file fails non-zero and says why (exit $RC)"
else
    bad "an empty --mint-file exited $RC and said: $OUT"
fi
"$PAYER_RUN" --mint-file "$RUN_DIR/nope.txt" --module-url "$BASE" >/dev/null 2>&1
RC=$?
[ "$RC" -eq 1 ] && ok "a MISSING --mint-file fails 1 (a run failure, not a usage error)" \
    || bad "a missing --mint-file exited $RC, want 1"
grep -q 'PAYER_EXIT=1' <("$PAYER_RUN" --mint-file "$RUN_DIR/nope.txt" --module-url "$BASE" 2>/dev/null) \
    && ok "PAYER_EXIT is printed on the failure path too" \
    || bad "PAYER_EXIT is missing on the failure path"

# ── 5. THE RUN: the lane's exact invocation ─────────────────────────────────
say "5. run the driver with the lane's exact flags"
PAYER_LOG="$RUN_DIR/payer.log"
# Armed for the WHOLE run: the funding swap legitimately hits the mint during
# open, and this is how we prove the driver's mint traffic goes through the
# counted path at all. If the count here were zero, "zero calls between open and
# settle" (§6) would be a statement about a wire nobody was watching — the exact
# defect the in-process mint caused.
proxy_ctl reset >/dev/null && proxy_ctl arm >/dev/null
(
    cd "$RUN_DIR" || exit 1
    "$PAYER_RUN" --mint-url "$PROXIED_MINT" --module-url "$BASE" --flow open-settle \
        > "$PAYER_LOG" 2>&1
)
PAYER_RC=$?
OPEN_PHASE_COUNT="$(proxy_ctl count | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])' 2>/dev/null)"
proxy_ctl disarm >/dev/null
echo "$PAYER_RC" > "$RUN_DIR/payer.rc"

if grep -q '^PAYER_OPEN_OK channel_id=' "$PAYER_LOG"; then
    ok "PAYER_OPEN_OK emitted"
else
    bad "no PAYER_OPEN_OK; driver exited $PAYER_RC. --- payer.log ---"
    sed -n '1,60p' "$PAYER_LOG"
fi

OPEN_COUNT="$(grep -c 'PAYER_OPEN_OK' "$PAYER_LOG" || true)"
[ "$OPEN_COUNT" = "1" ] && ok "PAYER_OPEN_OK appears exactly once (count=$OPEN_COUNT)" \
    || bad "PAYER_OPEN_OK appears $OPEN_COUNT times, want 1"

CHANNEL_ID="$(sed -n 's/^PAYER_OPEN_OK channel_id=\([0-9a-fA-F]*\).*/\1/p' "$PAYER_LOG" | head -1)"

if grep -qE '^PAYER_SETTLE_OK channel_id=[0-9a-fA-F]+ balance=[0-9]+ payee_credited=[0-9]+' "$PAYER_LOG"; then
    ok "PAYER_SETTLE_OK has the lane's exact shape"
    BAL="$(sed -n 's/.*balance=\([0-9]*\).*/\1/p' "$PAYER_LOG" | tail -1)"
    CRED="$(sed -n 's/.*payee_credited=\([0-9]*\).*/\1/p' "$PAYER_LOG" | tail -1)"
    [ "$BAL" = "$CRED" ] && ok "the payee credited what the payer signed (balance=$BAL)" \
        || bad "balance=$BAL but payee_credited=$CRED"
else
    bad "no well-formed PAYER_SETTLE_OK; driver log:"
    sed -n '1,80p' "$PAYER_LOG"
fi

grep -q '^PAYER_EXIT=0$' "$PAYER_LOG" \
    && ok "PAYER_EXIT=0 on the success path" \
    || bad "PAYER_EXIT=0 absent (log says: $(grep -m1 '^PAYER_EXIT=' "$PAYER_LOG" || echo none))"

[ "$PAYER_RC" -eq 0 ] && ok "the process exit status agrees (0)" \
    || bad "the process exited $PAYER_RC"

if [ -n "$CHANNEL_ID" ]; then
    STATUS="$(curl -fsS --max-time 5 "$BASE/spilman/status?channel_id=$CHANNEL_ID" 2>/dev/null)"
    HELD="$(printf '%s' "$STATUS" | python3 -c \
        'import json,sys; d=json.load(sys.stdin); print(d.get("balance"))' 2>/dev/null)"
    if [ "${HELD:-null}" = "${BAL:-x}" ]; then
        ok "GET /spilman/status reports the settled balance ($HELD)"
    else
        bad "status reports balance=${HELD:-<none>}, driver signed ${BAL:-<none>} — $STATUS"
    fi
fi

# The funding swap legitimately hits the mint DURING open. That is not assertion
# F; F is about the window after the channel is open. What this proves is that the
# driver's mint traffic goes through the counted path at all — otherwise a zero
# count in §6 would mean "we were not looking", not "there were no calls".
if [ "${OPEN_PHASE_COUNT:-0}" -ge 1 ]; then
    ok "the driver's mint traffic is visible to the proxy (armed whole-run count=$OPEN_PHASE_COUNT)"
else
    bad "the proxy saw ZERO traffic during a whole run: the driver did not fund through --mint-url, so §6 measures nothing"
fi

# ── 6. assertion F, measured ────────────────────────────────────────────────
# An independent second run, with the proxy ARMED once the channel is open and
# DISARMED the moment the driver exits, measures the same window the lane
# measures: after open, until settle. Because the driver's funding goes through
# the proxy, a mint call in that window would be counted — so a zero here is a
# measurement, not an absence of evidence.
say "6. assertion F: zero mint calls between open and settle"

# Positive control FIRST: an armed proxy must count a request it forwards. Without
# this, a "zero" below could be a broken counter rather than a quiet payer — the
# same class of bug as the in-process mint, one layer down.
proxy_ctl reset >/dev/null && proxy_ctl arm >/dev/null
curl -fsS --max-time 5 "$PROXIED_MINT/v1/info" >/dev/null 2>&1
CONTROL_COUNT="$(proxy_ctl count | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])' 2>/dev/null)"
proxy_ctl disarm >/dev/null
[ "${CONTROL_COUNT:-0}" -ge 1 ] \
    && ok "counter has teeth: an armed request to the mint was counted (count=$CONTROL_COUNT)" \
    || bad "an armed request to the mint was NOT counted (count=${CONTROL_COUNT:-?}) — a later zero would prove nothing"

proxy_ctl reset >/dev/null
(
    cd "$RUN_DIR" || exit 1
    "$PAYER_RUN" --mint-url "$PROXIED_MINT" --module-url "$BASE" --flow open-settle \
        > "$RUN_DIR/payer2.log" 2>&1
) &
SECOND_PID=$!
STARTED_PIDS+=("$SECOND_PID")

# Arm only once the channel is open, so the funding swap is excluded the way the
# lane excludes it; reset at the same instant so the counter is zero at the start
# of the measured window.
for _ in $(seq 1 300); do
    grep -q '^PAYER_OPEN_OK' "$RUN_DIR/payer2.log" 2>/dev/null && break
    kill -0 "$SECOND_PID" 2>/dev/null || break
    sleep 0.05
done
proxy_ctl reset >/dev/null && proxy_ctl arm >/dev/null   # the open→settle window starts here
wait "$SECOND_PID" 2>/dev/null
SECOND_RC=$?
proxy_ctl disarm >/dev/null
WINDOW_COUNT="$(proxy_ctl count | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])' 2>/dev/null)"
WINDOW_RECS="$(proxy_ctl records 2>/dev/null)"

if [ "$SECOND_RC" -eq 0 ] && grep -q '^PAYER_SETTLE_OK' "$RUN_DIR/payer2.log"; then
    ok "the second run also settled (exit 0)"
    [ "${WINDOW_COUNT:-1}" = "0" ] \
        && ok "the counting proxy saw ZERO mint calls between open and settle" \
        || bad "the counting proxy saw ${WINDOW_COUNT} mint call(s): $WINDOW_RECS"
else
    bad "the second run did not settle (exit $SECOND_RC); window count=${WINDOW_COUNT:-?}"
    sed -n '1,60p' "$RUN_DIR/payer2.log"
fi

# ── 7. the --payee alias and --mint-file, still real ────────────────────────
say "7. --payee is still an alias for --module-url, and --mint-file funded this run"
(
    cd "$RUN_DIR" || exit 1
    "$PAYER_RUN" --mint-file "$RUN_DIR/mint.txt" --payee "$BASE" > "$RUN_DIR/payer3.log" 2>&1
)
THIRD_RC=$?
if [ "$THIRD_RC" -eq 0 ] && grep -q '^PAYER_SETTLE_OK' "$RUN_DIR/payer3.log"; then
    ok "--payee + --mint-file ran a full open-settle (default flow) and settled"
else
    bad "--payee + --mint-file run exited $THIRD_RC; log:"
    sed -n '1,60p' "$RUN_DIR/payer3.log"
fi
grep -q '^PAYER_EXIT=0$' "$RUN_DIR/payer3.log" \
    && ok "PAYER_EXIT=0 on the --payee path too" \
    || bad "no PAYER_EXIT=0 on the --payee path"

# ── 8. no in-process mint left in the driver ────────────────────────────────
# The whole point: the example must not spawn its own mint, or the lane's counting
# proxy is bypassed and assertion F cannot fail. This is checked in the SOURCE, so
# a future edit cannot quietly reintroduce it.
say "8. the example no longer references the in-process test mint"
if grep -qE 'cdk_spilman_test_mint|TestMintHelper|serve_existing_mint' \
        "$REPO/examples/spilman_payer.rs"; then
    bad "examples/spilman_payer.rs still references the in-process test mint"
else
    ok "examples/spilman_payer.rs has no in-process mint reference"
fi
# ...and it must not have compiled one in, which a symbol check on the binary shows.
if command -v strings >/dev/null && strings "$PAYER_RUN" | grep -qi 'cdk-spilman-test-mint'; then
    bad "the driver binary still contains cdk-spilman-test-mint symbols"
else
    ok "the driver binary carries no cdk-spilman-test-mint symbols"
fi
# A normal build must not carry the mint leg at all: it is behind `payer-driver`.
if ( cd "$REPO" && cargo build --lib 2>/dev/null ); then
    lib_rlib="$(find "$REPO/target/debug/deps" -maxdepth 1 -name 'libtollgate_module_basic_rust-*.rlib' -newermt '-60 seconds' | head -1)"
    if [ -n "$lib_rlib" ] && strings "$lib_rlib" | grep -q 'spilman_mint'; then
        bad "a plain (no-feature) lib build still contains the mint_http leg"
    else
        ok "a plain cargo build --lib does not compile the mint_http leg"
    fi
else
    bad "cargo build --lib (no features) failed"
fi

# ── summary ─────────────────────────────────────────────────────────────────
say "summary"
printf '  %d checks, %d failed\n' "$CHECKS" "$FAILURES"
printf '  artifacts: %s\n' "$RUN_DIR"
if [ "$FAILURES" -eq 0 ]; then
    echo "  RESULT: PASS — the driver funds at a remote mint over HTTP, the marker"
    echo "          contract holds, and the open→settle window is measurably mint-free."
    exit 0
fi
echo "  RESULT: FAIL — see the FAIL lines above."
echo "  logs: payer=$RUN_DIR/payer.log payer2=$RUN_DIR/payer2.log mod=$RUN_DIR/mod.log mint=$RUN_DIR/mint.log"
exit 1
