//! `subscribe` — stateless, database-free subscription service.
//!
//! Users pay ERC-20 tokens on-chain to rent configurable jobs that run on the
//! server and report results to their Telegram chat.
//!
//! * [`store`] — one small JSON file per user (`telegram_id` + job config);
//! * [`bip32`] — per-user payment addresses derived from a watch-only xpub;
//! * [`ledger`] — subscription state recomputed from `Transfer` logs ([`eth`]);
//! * [`jobs`] — HTTP / shell job execution with bounded time and output;
//! * [`bot`] + [`telegram`] — the Telegram interface and job scheduler;
//! * [`server`] — the runtime that ties everything together.

pub mod bip32;
pub mod bot;
pub mod config;
pub mod eth;
pub mod fmt;
pub mod jobs;
pub mod ledger;
pub mod server;
pub mod store;
pub mod telegram;

/// HTTP client builder shared by every outbound client.
///
/// Installs the process-wide rustls crypto provider (ring) on first use, as
/// required by reqwest's `rustls-no-provider` build. Certificates are
/// verified against the OS trust store.
pub fn http_client() -> reqwest::ClientBuilder {
    static TLS: std::sync::Once = std::sync::Once::new();
    TLS.call_once(|| {
        // Err only means another provider is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder().user_agent(concat!("subscribe/", env!("CARGO_PKG_VERSION")))
}

/// Current wall-clock time in unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
