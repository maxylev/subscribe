//! Server runtime: wires the components together and runs the event loop.
//!
//! * A polling task long-polls Telegram `getUpdates` and forwards updates
//!   over a bounded channel. The offset advances only after an update is
//!   handed off, so no update is lost; Telegram's `retry_after` is honored.
//! * Each update is handled in its own task, so one slow command (e.g. a
//!   `/run`) never blocks other users.
//! * A scheduler tick every `cron_interval_secs` runs due jobs of active
//!   subscriptions. Ticks never overlap.
//! * SIGINT/SIGTERM stop polling and scheduling, then wait (bounded) for
//!   running jobs to finish.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::bot::Bot;
use crate::config::Config;
use crate::ledger::Ledger;
use crate::store::Store;
use crate::telegram::{ApiError, TgClient, Update};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
const POLL_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Build all components from `cfg` and serve until a shutdown signal.
pub async fn serve(cfg: Config) -> Result<()> {
    let token = cfg
        .bot_token
        .clone()
        .context("bot_token is required (config [server] bot_token or SUBSCRIBE_BOT_TOKEN)")?;
    let tg = TgClient::new(&cfg.tg_base_url, token, cfg.poll_secs)?;
    let store = Store::open(&cfg.data_dir, cfg.xpub.clone())?;
    let ledger = Ledger::new(&cfg.chains, cfg.use_chain_time)?;
    let bot = Arc::new(Bot::new(cfg, store, ledger, tg)?);

    let me = bot
        .telegram()
        .get_me()
        .await
        .context("telegram getMe failed (check the bot token)")?;
    tracing::info!(
        "connected as @{}",
        me.username.as_deref().unwrap_or("unknown")
    );
    tracing::info!("watching {} chain(s)", bot.config().chains.len());
    for (name, id) in bot.ledger().chain_ids().await {
        match id {
            Ok(id) => tracing::info!("chain {name}: id {id}"),
            Err(e) => tracing::warn!("chain {name}: unreachable: {e:#}"),
        }
    }

    let (tx, mut rx) = mpsc::channel::<Update>(256);
    let poller = tokio::spawn(poll(bot.telegram().clone(), bot.config().poll_secs, tx));

    let mut ticker = tokio::time::interval(Duration::from_secs(bot.config().cron_interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let ticking = Arc::new(AtomicBool::new(false));
    let mut tasks = JoinSet::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(update) = rx.recv() => {
                let bot = Arc::clone(&bot);
                tasks.spawn(async move { bot.handle_update(update).await });
            }
            _ = ticker.tick() => {
                if ticking.swap(true, Ordering::AcqRel) {
                    tracing::warn!("previous tick still running; skipping");
                    continue;
                }
                let bot = Arc::clone(&bot);
                let ticking = Arc::clone(&ticking);
                tasks.spawn(async move {
                    bot.tick().await;
                    ticking.store(false, Ordering::Release);
                });
            }
            Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = res {
                    tracing::error!("task failed: {e}");
                }
            }
        }
    }

    tracing::info!("shutting down");
    poller.abort();
    let drain = async {
        while tasks.join_next().await.is_some() {}
        bot.drain().await;
    };
    if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
        tracing::warn!("jobs still running after {SHUTDOWN_GRACE:?}; aborting them");
    }
    Ok(())
}

/// Long-poll Telegram forever, forwarding updates to `tx`.
async fn poll(tg: TgClient, poll_secs: u64, tx: mpsc::Sender<Update>) {
    let mut offset = 0i64;
    loop {
        match tg.get_updates(offset, poll_secs).await {
            Ok(updates) => {
                for u in updates {
                    let next = u.update_id + 1;
                    if tx.send(u).await.is_err() {
                        return;
                    }
                    offset = next;
                }
            }
            Err(e) => {
                let wait = e
                    .downcast_ref::<ApiError>()
                    .and_then(|a| a.retry_after)
                    .map_or(POLL_ERROR_BACKOFF, Duration::from_secs);
                tracing::warn!("getUpdates: {e:#}; retrying in {wait:?}");
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Resolves on SIGINT or (on Unix) SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("installing SIGINT handler: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("installing SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => tracing::info!("SIGINT received"),
        () = term => tracing::info!("SIGTERM received"),
    }
}
