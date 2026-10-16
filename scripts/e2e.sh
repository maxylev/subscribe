#!/usr/bin/env bash
# End-to-end test: two anvil chains with different per-token rates, a mock
# Telegram API and a mock HTTP job target. Every check is a hard assertion.
#
# Covers: bot commands and buttons, smart-quoted /config, multi-chain payment
# aggregation, scheduled job delivery (with URL-encoded input), lapse and
# top-up semantics, chain-failure isolation, token redaction and SIGTERM.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/common.sh
source scripts/common.sh

require_tools anvil cast forge jq python3 cargo

WORK="$(mktemp -d "${TMPDIR:-/tmp}/subscribe-e2e.XXXXXX")"
SERVER_PID=""
on_exit() {
    local status=$?
    cleanup_pids
    if ((status == 0)); then
        rm -rf "$WORK"
    else
        echo "artifacts kept in $WORK" >&2
    fi
}
trap on_exit EXIT

CONFIG="$WORK/config.toml"
SENT="$WORK/tg_sent.jsonl"
SERVER_LOG="$WORK/server.log"
TG_ID=424242
BOT_TOKEN="e2e-secret-token:DO-NOT-LOG"
UPDATE_ID=0
touch "$SENT"

# inject <update-json>: queue an update for the mock's getUpdates.
inject() {
    UPDATE_ID=$((UPDATE_ID + 1))
    jq -c --argjson id "$UPDATE_ID" '. + {update_id: $id}' <<<"$1" \
        >"$WORK/inbox/$(printf '%06d' "$UPDATE_ID").json"
}

send_text() {
    inject "$(jq -nc --arg t "$1" --argjson u "$TG_ID" \
        '{message: {message_id: 1, date: 0, text: $t,
          from: {id: $u, is_bot: false, first_name: "Tester"},
          chat: {id: $u, type: "private"}}}')"
}

tap() {
    inject "$(jq -nc --arg d "$1" --argjson u "$TG_ID" \
        '{callback_query: {id: ("cb-" + $d), data: $d,
          from: {id: $u, is_bot: false, first_name: "Tester"},
          message: {message_id: 2, date: 0, chat: {id: $u, type: "private"}}}}')"
}

# expect_reply <fixed-string> <description>: wait for a bot message.
expect_reply() {
    wait_for 15 "$2" grep -qF -- "$1" "$SENT"
    ok "$2"
}

check_user() { "$BIN" check 0 -c "$CONFIG" >"$WORK/check.txt"; }

echo "== build"
cargo build --locked --quiet
XPUB=$("$BIN" genxpub --seed-hex "$SEED_HEX")

echo "== chains"
start_anvil "$WORK/anvil-a.log"
RPC_A=$ANVIL_RPC
start_anvil "$WORK/anvil-b.log"
RPC_B=$ANVIL_RPC
ANVIL_B_PID=${PIDS[${#PIDS[@]} - 1]}
TOKEN_A=$(deploy_token "$RPC_A")
TOKEN_B=$(deploy_token "$RPC_B")
ok "SubToken deployed on $RPC_A ($TOKEN_A) and $RPC_B ($TOKEN_B)"

echo "== mocks"
HTTP_PORT=$(free_port)
TG_PORT=$(free_port)
python3 scripts/mock_http.py "$HTTP_PORT" >"$WORK/mock_http.log" 2>&1 &
track $!
python3 scripts/mock_tg.py "$TG_PORT" "$WORK" >"$WORK/mock_tg.log" 2>&1 &
track $!
wait_for 10 "mock telegram" test -d "$WORK/inbox"

cat >"$CONFIG" <<EOF
[server]
data_dir = "$WORK/data"
cron_interval_secs = 1
poll_secs = 1
job_timeout_secs = 10
use_chain_time = true
bot_token = "$BOT_TOKEN"
tg_base_url = "http://127.0.0.1:$TG_PORT"
xpub = "$XPUB"

[[chain]]
name = "anvil-a"
rpc_url = "$RPC_A"
log_range = 3

[[chain.token]]
address = "$TOKEN_A"
symbol = "SUBA"
decimals = 18
rate_per_minute = "1000000000000000000"

[[chain]]
name = "anvil-b"
rpc_url = "$RPC_B"

[[chain.token]]
address = "$TOKEN_B"
symbol = "SUBB"
decimals = 18
rate_per_minute = "2000000000000000000"
EOF

echo "== server"
RUST_LOG=info,subscribe=debug "$BIN" serve -c "$CONFIG" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
track "$SERVER_PID"
wait_for 15 "server connected" grep -qF "connected as @mockbot" "$SERVER_LOG"
assert_grep "$SERVER_LOG" "watching 2 chain(s)" "server watches both chains"

echo "== bot commands before payment"
send_text "/start"
expect_reply "Welcome!" "/start replies with a welcome"
ADDR=$("$BIN" addr 0 -c "$CONFIG")
assert_grep "$SENT" "$ADDR" "/start shows the derived EIP-55 address of user #0"
assert_grep "$SENT" "SUBB</b> — 2 SUBB/min" "/start lists per-token prices"

# Smart quotes, as produced by mobile keyboards; the input needs URL-encoding.
# shellcheck disable=SC1111 # the unicode quotes are the point of this test
send_text "/config {“name”:“demo”,“kind”:“http”,“interval_secs”:2,“input”:{“keyword”:“hello sub&x”},“http”:{“url”:“http://127.0.0.1:$HTTP_PORT/echo?q={keyword}”}}"
expect_reply "Job saved: <code>demo</code> (http, every 2s)" "/config accepts smart-quoted json"
send_text "/config {not json"
expect_reply "invalid job json" "/config rejects malformed json"
send_text "/status"
expect_reply "no payments yet" "/status before payment"
send_text "/config"
expect_reply "Job config" "bare /config shows examples"
tap "run"
expect_reply "subscription is not active" "Run button refuses inactive subscriptions"
assert_grep "$SENT" '"callback_query_id": "cb-run"' "button taps are acknowledged"
send_text "/start@mockbot"
wait_for 15 "second welcome" bash -c "test \"\$(grep -cF 'address (#0)' '$SENT')\" -eq 2"
ok "/start@bot keeps the same index"
check_user
assert_grep "$WORK/check.txt" "active: false" "CLI: inactive before payment"

echo "== multi-chain payments"
pay "$RPC_A" "$TOKEN_A" "$ADDR" 50 # 50 SUBA @ 1/min = 50 min
pay "$RPC_B" "$TOKEN_B" "$ADDR" 20 # 20 SUBB @ 2/min = 10 min
# Push chain A past the log_range=3 window boundary.
for _ in 1 2 3 4; do cast rpc --rpc-url "$RPC_A" evm_mine >/dev/null; done
check_user
assert_grep "$WORK/check.txt" "active: true" "CLI: active after payments"
assert_grep "$WORK/check.txt" "paid_secs: 3600" "50 SUBA + 20 SUBB aggregate to 60 min"
assert_grep "$WORK/check.txt" "anvil-a/SUBA: 50 raw=50000000000000000000 transfers=1 secs=3000" "per-token totals on chain A"
assert_grep "$WORK/check.txt" "anvil-b/SUBB: 20 raw=20000000000000000000 transfers=1 secs=600" "per-token totals on chain B"

expect_reply "hello sub&amp;x" "scheduled job report delivered (input URL-encoded and echoed)"
assert_grep "$SENT" "<b>demo</b> finished in" "report is formatted"
send_text "/status"
expect_reply "✅ active" "/status after payment"

echo "== lapse and top-up"
cast rpc --rpc-url "$RPC_A" evm_increaseTime 7200 >/dev/null
cast rpc --rpc-url "$RPC_A" evm_mine >/dev/null
check_user
assert_grep "$WORK/check.txt" "active: false" "expired after a 2h time warp"
pay "$RPC_A" "$TOKEN_A" "$ADDR" 10
check_user
assert_grep "$WORK/check.txt" "active: true" "a top-up after the lapse reactivates"
REMAINING=$(sed -n 's/^remaining_secs: //p' "$WORK/check.txt")
((REMAINING > 500 && REMAINING <= 600)) || die "top-up should start a fresh 10 min period, got ${REMAINING}s"
ok "top-up starts a new 10 min period (${REMAINING}s left)"
assert_grep "$WORK/check.txt" "paid_secs: 4200" "all paid time is accounted"

echo "== chain failure isolation"
kill "$ANVIL_B_PID"
wait_for 10 "chain B down" bash -c "! cast chain-id --rpc-url '$RPC_B'"
check_user
assert_grep "$WORK/check.txt" "active: true" "still active from chain A"
assert_grep "$WORK/check.txt" "anvil-b: ERROR" "chain B failure is reported"

echo "== shutdown and hygiene"
kill -TERM "$SERVER_PID"
wait_for 25 "server exit" bash -c "! kill -0 $SERVER_PID"
assert_grep "$SERVER_LOG" "SIGTERM received" "SIGTERM triggers graceful shutdown"
if grep -qF "$BOT_TOKEN" "$SERVER_LOG"; then die "bot token leaked into the server log"; fi
ok "bot token never logged"
if grep -q "panicked" "$SERVER_LOG"; then die "server panicked"; fi
ok "no panics"

echo
echo "ALL E2E CHECKS PASSED"
