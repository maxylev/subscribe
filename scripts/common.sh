#!/usr/bin/env bash
# Shared helpers for the e2e / demo / smoke scripts. Source, don't execute.

# Anvil's first default account (public test key — never use on a real chain).
export KEY0=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
# Public test seed ("subscribe test seed") used by the README quickstart.
export SEED_HEX=73756273637269626520746573742073656564
export BIN=./target/debug/subscribe

# Load ./.env (e.g. SUBSCRIBE_BOT_TOKEN) without overriding the environment.
if [[ -f .env ]]; then
    while IFS='=' read -r key value; do
        key="${key#"${key%%[![:space:]]*}"}" # trim leading whitespace
        [[ "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || continue
        [[ -n "${!key:-}" ]] || export "$key=$value"
    done <.env
fi

PIDS=()

# Kill every background process registered with `track`.
cleanup_pids() {
    local pid
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}

track() { PIDS+=("$1"); }

die() {
    echo "FAIL: $*" >&2
    exit 1
}

ok() { echo "  ok - $*"; }

require_tools() {
    local t
    for t in "$@"; do
        command -v "$t" >/dev/null || die "missing required tool: $t"
    done
}

# Print a free TCP port on 127.0.0.1.
free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

# wait_for <timeout-secs> <description> <command...>: poll until the command succeeds.
wait_for() {
    local timeout="$1" what="$2"
    shift 2
    local deadline=$((SECONDS + timeout))
    until "$@" >/dev/null 2>&1; do
        ((SECONDS < deadline)) || die "timed out after ${timeout}s waiting for: $what"
        sleep 0.25
    done
}

# assert_grep <file> <fixed-string> <description>
assert_grep() {
    grep -qF -- "$2" "$1" || {
        echo "---- $1 ----" >&2
        tail -n 40 "$1" >&2
        die "$3 (expected to find: $2)"
    }
    ok "$3"
}

# start_anvil <log-file>: start anvil on a free port and set ANVIL_RPC.
# (Not meant for $(...): the PID must be tracked in the calling shell.)
start_anvil() {
    local port
    port=$(free_port)
    anvil --port "$port" >"$1" 2>&1 &
    track $!
    ANVIL_RPC="http://127.0.0.1:$port"
    wait_for 20 "anvil on :$port" cast chain-id --rpc-url "$ANVIL_RPC"
}

# deploy_token <rpc-url>: deploy contracts/SubToken.sol, echo its address.
deploy_token() {
    forge create contracts/SubToken.sol:SubToken \
        --rpc-url "$1" --private-key "$KEY0" --broadcast --json 2>/dev/null |
        jq -er '.deployedTo'
}

# pay <rpc-url> <token> <to> <whole-tokens>: transfer an 18-decimals token.
pay() {
    cast send --rpc-url "$1" --private-key "$KEY0" "$2" \
        "transfer(address,uint256)" "$3" "$(cast to-wei "$4")" >/dev/null
}
