# subscribe

[![CI](https://github.com/maxylev/subscribe/actions/workflows/ci.yml/badge.svg)](https://github.com/maxylev/subscribe/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/maxylev/subscribe)](https://github.com/maxylev/subscribe/releases)

A stateless, database-free subscription service in Rust. Users pay ERC-20
tokens on-chain to rent **jobs** — HTTP requests or shell commands — that run
on your server and report results to their Telegram chat.

- **No database.** Each user is one small JSON file (`data/{index}.json`:
  Telegram id + job config). Payment truth lives on-chain and is recomputed,
  never stored.
- **Watch-only.** The server holds only an **xpub** and derives every user's
  payment address from it (`m/44'/60'/0'/0/{index}`). The seed stays offline.
- **Multi-chain, multi-token.** Any number of EVM chains, each accepting any
  number of ERC-20 tokens with its own price.
- **Fair clock.** Paying while active extends the current period; paying
  after a lapse starts a new one. Paid time is never lost.
- **Bounded jobs.** Timeouts are capped by the server, output is capped while
  it is read, and concurrency is limited.

## How it works

```
Telegram ──/start──▶ bot ──▶ address = xpub/index  ◀── user pays ERC-20 (any chain)
                      │
            data/{index}.json                     Transfer logs (eth_getLogs)
           (telegram id + job)                             │
                      │                                    ▼
                      └──────▶ scheduler tick ──▶ ledger: payments → expiry
                                     │ active & due?
                                     ▼
                               run job (http/shell) ──▶ report to Telegram
```

For every user address and accepted token, the ledger reads `Transfer`
logs and folds them in block-time order:

```text
duration = amount * 60 / rate_per_minute                  (seconds)
expiry   = max(expiry, payment_time) + duration           (per payment)
active   = now < expiry        now = newest block time across chains
```

Logs are cached in memory per address with a block cursor, so each tick only
queries new blocks; a restart simply rescans. Blocks newer than
`head - confirmations` are ignored, so reorganized-out payments are never
credited. A chain that is down is reported per chain and keeps the payments
already confirmed on it; healthy chains are unaffected.

## Quickstart (local, ~1 minute)

Requires Rust (the toolchain is pinned in `rust-toolchain.toml`) and
[Foundry](https://getfoundry.sh) (`anvil`, `forge`, `cast`), plus `jq` and `python3` for the scripts.

```bash
# Real Telegram bot against a local anvil chain (get a token from @BotFather):
SUBSCRIBE_BOT_TOKEN=<token> ./scripts/demo.sh

# Or the fully automated end-to-end test (two chains, mock Telegram, no token):
./scripts/e2e.sh
```

`demo.sh` prints the `cast send` command to pay. Then in Telegram:

1. `/start` → your payment address and the accepted tokens and prices.
2. Pay, e.g. 50 SUB = 50 minutes at `1 SUB/min`.
3. `/config {"name":"demo","kind":"http","interval_secs":30,"input":{"q":"rust"},"http":{"url":"https://example.com/?q={q}"}}`
4. `/status` → `✅ active — 49m left`; the report arrives every 30 s.
   Use `/run` (or the ▶️ button) to trigger the job right away.

## Production

```bash
# 1. Offline: create a seed + account xpub (or derive from an existing seed).
subscribe genxpub                 # seed → stderr (back it up), xpub → stdout

# 2. Configure: copy config.example.toml, set xpub, enable chains/tokens and
#    set from_block to the block you start accepting payments at.
cp config.example.toml config.toml

# 3. Run.
SUBSCRIBE_BOT_TOKEN=<token> subscribe serve -c config.toml
```

### Docker

Multi-arch images (`linux/amd64`, `linux/arm64`) are published to GHCR on
every release. The image runs as a non-root user (uid 10001) and stops
gracefully on `SIGTERM`.

```bash
docker run -d --name subscribe --restart unless-stopped \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  -v subscribe-data:/app/data \
  -e SUBSCRIBE_BOT_TOKEN=<token> \
  ghcr.io/maxylev/subscribe:latest
```

`docker build .` builds the same image from source.

## Bot commands

| Command | Effect |
|---|---|
| `/start` | creates your index; shows your payment address and prices |
| `/status` | active state, time left, paid time, job, payments per chain/token |
| `/config <json>` | sets your job (`/config` alone shows examples) |
| `/run` | runs your job now (subscription must be active) |
| `/help` | command list |

Every reply carries inline buttons for Status, Run, Set job and Help.
`/cmd@yourbot` works in groups.

## Jobs

```json
{
  "name": "search",
  "kind": "http",
  "interval_secs": 300,
  "timeout_secs": 10,
  "input": {"keyword": "rust lang"},
  "http": {
    "method": "POST",
    "url": "https://api.example.com/search?q={keyword}",
    "headers": {"Authorization": "Bearer abc"},
    "body": "{\"query\": \"{keyword}\"}"
  }
}
```

```json
{
  "name": "report",
  "kind": "shell",
  "interval_secs": 600,
  "input": {"keyword": "rust lang"},
  "shell": {"cmd": "/usr/local/bin/my-script", "args": ["--q", "{keyword}"]}
}
```

| Field | Default | Notes |
|---|---|---|
| `name` | — | 1–64 chars of `A-Z a-z 0-9 - _ .` |
| `kind` | — | `http` or `shell` |
| `interval_secs` | `300` | time between scheduled runs |
| `timeout_secs` | server `job_timeout_secs` | capped at the server value |
| `input` | `{}` | values for `{field}` placeholders |
| `http.method` | `GET` | GET, POST, PUT, PATCH, DELETE, HEAD |

Placeholders work in `url` (values are percent-encoded), `headers`, `body`
and `args`. Shell jobs run `cmd` directly with `args` (no shell
interpolation) and get the input JSON in `SUBSCRIBE_INPUT`. Reports contain
the HTTP status or exit code plus output, truncated to `job_max_output`
bytes. Smart quotes from mobile keyboards are accepted.

## Configuration

See [`config.example.toml`](config.example.toml) for every key and its
default. Summary:

| `[server]` key | Default | Env override |
|---|---|---|
| `data_dir` | `data` | `SUBSCRIBE_DATA_DIR` |
| `cron_interval_secs` | `60` | `SUBSCRIBE_CRON_INTERVAL_SECS` |
| `poll_secs` | `25` | `SUBSCRIBE_POLL_SECS` |
| `job_timeout_secs` | `15` | `SUBSCRIBE_JOB_TIMEOUT_SECS` |
| `job_max_output` | `3500` | `SUBSCRIBE_JOB_MAX_OUTPUT` |
| `max_concurrent_jobs` | `4` | `SUBSCRIBE_MAX_CONCURRENT_JOBS` |
| `use_chain_time` | `true` | `SUBSCRIBE_USE_CHAIN_TIME` |
| `bot_token` | — | `SUBSCRIBE_BOT_TOKEN` |
| `tg_base_url` | `https://api.telegram.org` | `SUBSCRIBE_TG_BASE_URL` |
| `xpub` (required) | — | `SUBSCRIBE_XPUB` |
| `from_block` | `0` | `SUBSCRIBE_FROM_BLOCK` |
| `log_range` | `10000` | — |
| `confirmations` | `0` | — |

Each `[[chain]]` has `name`, `rpc_url` and optional `from_block`, `log_range`,
`confirmations` and `enabled`. Each `[[chain.token]]` has `address`, `symbol`,
`decimals`, `rate_per_minute` (an integer, or a string for values beyond 2^63)
and optional `enabled`. Unknown keys are errors. `SUBSCRIBE_CONFIG` sets the
default `-c` path. Logging uses `RUST_LOG` (default `info`).

## CLI

```
subscribe serve   [-c config.toml]              run the server
subscribe genxpub [--seed-hex <hex>] [--path m/44'/60'/0'/0]
subscribe addr  <index> [-c config.toml]        payment address of a user
subscribe users         [-c config.toml]        all users with state
subscribe check <index> [-c config.toml]        detailed state of a user
```

## Security

- **Watch-only.** The seed is never needed by the server; keep it offline.
  Refunds are done offline from the seed.
- **Never delete or renumber user files.** Indexes are never reused, so two
  users can never share a funded address. Losing `data/` loses job
  configuration only, never payment accounting.
- **Jobs are code execution by design.** Shell jobs run arbitrary commands and
  HTTP jobs can reach your internal network. Accept only users you trust, and
  run the container with minimal privileges and network access.
- **Secrets stay out of logs.** The bot token and RPC URLs, which may embed
  API keys, are stripped from every error message.
- **Reorgs.** Set `confirmations` on public chains so a payment is credited
  only once it is final.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test                                 # unit tests (BIP-32/EIP-55 vectors, ledger, jobs…)
cargo test --test live -- --ignored        # live check against public Arbitrum RPC
./scripts/e2e.sh                           # two anvil chains + mock Telegram, full flow
SUBSCRIBE_BOT_TOKEN=<token> ./scripts/bot_smoke.sh   # live Telegram smoke test
```

Layout: `bip32` (keys and addresses), `config`, `eth` (JSON-RPC), `ledger`
(payments → subscription), `store` (user files), `jobs` (execution),
`telegram` (Bot API client), `bot` (commands and scheduling), `server`
(runtime and shutdown), `fmt` (rendering).

## Releasing

Releases are tag-driven:

1. Add notes under `## [Unreleased]` in [`CHANGELOG.md`](CHANGELOG.md).
2. Run `scripts/release.sh <version>` on an up-to-date `main` (`--dry-run`
   to preview). It bumps `Cargo.toml`/`Cargo.lock`, dates the changelog
   section, runs fmt, clippy and tests, commits, creates an annotated `v<version>` tag,
   and pushes branch and tag atomically.
3. The [Release workflow](.github/workflows/release.yml) then runs:
   1. the full CI;
   2. checks that the tag matches `Cargo.toml` and that the commit is on `main`;
   3. builds static binaries for `x86_64`/`aarch64` Linux (musl) and Apple
      Silicon;
   4. pushes a multi-arch image to GHCR with SBOM and provenance;
   5. publishes the GitHub release with checksums, build attestations and the
      changelog section as release notes.

   Tags with a pre-release suffix (`v1.2.0-rc.1`) are published as
   pre-releases and do not move `latest`.

## License

[MIT](LICENSE)
