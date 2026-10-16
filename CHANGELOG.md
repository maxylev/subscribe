# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-10-16

### Added

- Releases publish the crate to crates.io (`cargo install subscribe`), using Trusted Publishing with a `CARGO_REGISTRY_TOKEN` fallback; re-runs skip versions that are already published.
- CI verifies the crates.io package with `cargo publish --dry-run`.
- Scripts load `SUBSCRIBE_BOT_TOKEN` from a local `.env` file.

### Changed

- The packaged crate includes the live integration test.

## [0.1.0] - 2026-10-16

First public release.

### Added

- Telegram bot (`/start`, `/status`, `/config`, `/run`, `/help`, inline buttons, `/cmd@bot` in groups) with HTML replies.
- Watch-only BIP-32 address derivation with EIP-55 checksummed addresses; `genxpub` uses the OS RNG.
- Multi-chain, multi-token ERC-20 payment ledger with windowed `eth_getLogs` (`log_range`), `confirmations` and an incremental in-memory log cache.
- HTTP and shell jobs with `{field}` templating (percent-encoded in URLs), server-capped timeouts, bounded output and a concurrency limit (`max_concurrent_jobs`).
- Graceful shutdown on SIGINT/SIGTERM.
- `subscribe users` / `check` / `addr` operational commands (clap CLI).
- Strict end-to-end test (two anvil chains, mock Telegram) and an opt-in live test against public Arbitrum.
- Tag-driven release pipeline: verified tags, static musl and macOS binaries, checksums, build provenance, and multi-arch GHCR images with SBOM.

### Security

- Bot tokens and RPC URLs are stripped from all error messages and logs; the token is redacted from `Debug` output.
- The container runs as a non-root user.

[Unreleased]: https://github.com/maxylev/subscribe/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/maxylev/subscribe/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/maxylev/subscribe/releases/tag/v0.1.0
