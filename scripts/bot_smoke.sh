#!/usr/bin/env bash
# Live smoke test against the real Telegram Bot API: starts the server with
# your bot token for a few seconds and asserts that it authenticates, polls
# without errors and never logs the token.
#
#   SUBSCRIBE_BOT_TOKEN=<token> ./scripts/bot_smoke.sh
#
# Stop any other process polling the same bot first (Telegram allows one).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/common.sh
source scripts/common.sh

[[ -n "${SUBSCRIBE_BOT_TOKEN:-}" ]] || die "SUBSCRIBE_BOT_TOKEN is required"
require_tools anvil cast forge jq python3 cargo

WORK="$(mktemp -d "${TMPDIR:-/tmp}/subscribe-smoke.XXXXXX")"
trap 'cleanup_pids; rm -rf "$WORK"' EXIT
LOG="$WORK/server.log"

cargo build --locked --quiet
start_anvil "$WORK/anvil.log"
TOKEN=$(deploy_token "$ANVIL_RPC")
XPUB=$("$BIN" genxpub --seed-hex "$SEED_HEX")

cat >"$WORK/config.toml" <<EOF
[server]
data_dir = "$WORK/data"
cron_interval_secs = 5
poll_secs = 5
xpub = "$XPUB"

[[chain]]
name = "anvil"
rpc_url = "$ANVIL_RPC"

[[chain.token]]
address = "$TOKEN"
symbol = "SUB"
decimals = 18
rate_per_minute = "1000000000000000000"
EOF

"$BIN" serve -c "$WORK/config.toml" >"$LOG" 2>&1 &
track $!
wait_for 20 "telegram authentication" grep -qF "connected as @" "$LOG"
ok "authenticated: $(grep -o 'connected as @[A-Za-z0-9_]*' "$LOG" | head -1)"
assert_grep "$LOG" "chain anvil: id 31337" "anvil chain reachable"
sleep 12 # two long-poll cycles
if grep -qF "getUpdates:" "$LOG"; then
    tail -n 20 "$LOG" >&2
    die "getUpdates errors (is another process polling this bot?)"
fi
ok "long polling healthy"
if grep -qF "$SUBSCRIBE_BOT_TOKEN" "$LOG"; then die "bot token leaked into the log"; fi
ok "bot token never logged"
echo "SMOKE TEST PASSED"
