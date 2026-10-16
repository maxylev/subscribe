#!/usr/bin/env bash
# One-command local demo against a real Telegram bot: starts anvil, deploys
# SubToken, writes a config and runs the server until Ctrl-C.
#
#   SUBSCRIBE_BOT_TOKEN=<token> ./scripts/demo.sh
#
# Everything lives in a temp dir that is removed on exit. Pay with the
# command printed at startup (the address comes from the bot's /start).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/common.sh
source scripts/common.sh

[[ -n "${SUBSCRIBE_BOT_TOKEN:-}" ]] || die "SUBSCRIBE_BOT_TOKEN is required"
require_tools anvil cast forge jq python3 cargo

WORK="$(mktemp -d "${TMPDIR:-/tmp}/subscribe-demo.XXXXXX")"
trap 'cleanup_pids; rm -rf "$WORK"' EXIT

cargo build --locked --quiet
start_anvil "$WORK/anvil.log"
TOKEN=$(deploy_token "$ANVIL_RPC")
XPUB=$("$BIN" genxpub --seed-hex "$SEED_HEX")

cat >"$WORK/config.toml" <<EOF
[server]
data_dir = "$WORK/data"
cron_interval_secs = 10
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

cat <<EOF
chain:  $ANVIL_RPC
token:  $TOKEN
config: $WORK/config.toml

Message your bot /start, then pay (50 SUB = 50 minutes):
  cast send --rpc-url $ANVIL_RPC --private-key $KEY0 \\
    $TOKEN "transfer(address,uint256)" <address-from-bot> 50000000000000000000

Ctrl-C to stop.
EOF

"$BIN" serve -c "$WORK/config.toml"
