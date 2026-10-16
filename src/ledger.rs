//! Payment ledger: derives subscription state from on-chain `Transfer` logs.
//!
//! The server stores no payment state. For every user address and accepted
//! token the ledger reads `Transfer` logs and converts each payment into
//! subscription time:
//!
//! ```text
//! duration(payment) = amount * 60 / rate_per_minute        seconds
//! expiry            = fold over payments ordered by block time:
//!                     expiry = max(expiry, payment_time) + duration
//! active            = now < expiry
//! ```
//!
//! Paying while active extends the current period; paying after a lapse
//! starts a new period at the payment time, so no paid time is ever lost.
//!
//! Scanned logs are cached in memory per `(chain, token, address)` with a
//! block cursor, so each tick only queries blocks that are new since the
//! previous scan. The cache is a pure optimization: a restart rebuilds it
//! from the chain. Blocks newer than `head - confirmations` are not scanned,
//! which protects against crediting payments that are later reorganized out.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::task::JoinSet;

use crate::config::{Chain, Token};
use crate::eth::{Eth, Head};

/// One chain endpoint with its validated configuration.
struct ChainClient {
    cfg: Chain,
    eth: Eth,
}

/// A credited payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Payment {
    timestamp: u64,
    amount: u128,
}

/// Scan progress for one `(chain, token, address)`.
#[derive(Debug, Clone, Default)]
struct Cursor {
    /// Next block to scan.
    next_block: u64,
    payments: Vec<Payment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CursorKey {
    chain: usize,
    token: usize,
    address: String,
}

/// Chain heads fetched once per tick, aligned with the configured chains.
#[derive(Debug, Clone)]
pub struct Heads(Vec<Result<Head, String>>);

impl Heads {
    /// Newest block time across all reachable chains.
    pub fn newest_timestamp(&self) -> Option<u64> {
        self.0
            .iter()
            .filter_map(|h| h.as_ref().ok())
            .map(|h| h.timestamp)
            .max()
    }
}

/// Aggregated subscription state of one user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subscription {
    /// Total paid subscription time across all chains and tokens.
    pub paid_secs: u64,
    /// End of the current (or last) paid period; `None` before any payment.
    pub expiry: Option<u64>,
    /// The "now" the state was evaluated at.
    pub now: u64,
}

impl Subscription {
    /// Fold `(payment_time, duration_secs)` pairs into a subscription.
    pub fn from_payments(mut payments: Vec<(u64, u64)>, now: u64) -> Self {
        payments.sort_unstable();
        let mut expiry: Option<u64> = None;
        let mut paid_secs = 0u64;
        for (time, secs) in payments.into_iter().filter(|&(_, secs)| secs > 0) {
            paid_secs = paid_secs.saturating_add(secs);
            let start = expiry.map_or(time, |e| e.max(time));
            expiry = Some(start.saturating_add(secs));
        }
        Self {
            paid_secs,
            expiry,
            now,
        }
    }

    pub fn is_active(&self) -> bool {
        self.expiry.is_some_and(|e| self.now < e)
    }

    /// Seconds of paid time left (0 when inactive).
    pub fn remaining_secs(&self) -> u64 {
        self.expiry.map_or(0, |e| e.saturating_sub(self.now))
    }
}

/// Per-token payment totals.
#[derive(Debug, Clone)]
pub struct TokenReport {
    pub symbol: String,
    pub decimals: u8,
    pub transfers: usize,
    pub paid: u128,
    pub paid_secs: u64,
}

/// Per-chain scan result. A failing chain sets `error` but keeps every
/// payment that was already confirmed (cached) on it.
#[derive(Debug, Clone)]
pub struct ChainReport {
    pub name: String,
    pub tokens: Vec<TokenReport>,
    pub error: Option<String>,
}

/// Full scan result for one user.
#[derive(Debug, Clone)]
pub struct Scan {
    pub subscription: Subscription,
    pub chains: Vec<ChainReport>,
}

/// Subscription time bought by `amount` of a token priced at `rate_per_minute`.
pub fn duration_secs(amount: u128, rate_per_minute: u128) -> u64 {
    let secs = amount.saturating_mul(60) / rate_per_minute.max(1);
    u64::try_from(secs).unwrap_or(u64::MAX)
}

/// Multi-chain payment scanner with an in-memory log cache.
pub struct Ledger {
    chains: Vec<ChainClient>,
    use_chain_time: bool,
    cache: Mutex<HashMap<CursorKey, Cursor>>,
}

impl Ledger {
    pub fn new(chains: &[Chain], use_chain_time: bool) -> Result<Self> {
        let chains = chains
            .iter()
            .map(|cfg| {
                Ok(ChainClient {
                    eth: Eth::new(&cfg.rpc_url)
                        .with_context(|| format!("chain {}: rpc client", cfg.name))?,
                    cfg: cfg.clone(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            chains,
            use_chain_time,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Chain id of every chain (for startup diagnostics).
    pub async fn chain_ids(&self) -> Vec<(String, Result<u64>)> {
        let mut out = Vec::with_capacity(self.chains.len());
        for c in &self.chains {
            out.push((c.cfg.name.clone(), c.eth.chain_id().await));
        }
        out
    }

    /// Fetch every chain head concurrently.
    pub async fn heads(&self) -> Heads {
        let mut set = JoinSet::new();
        for (i, c) in self.chains.iter().enumerate() {
            let eth = c.eth.clone();
            set.spawn(async move { (i, eth.head().await) });
        }
        let mut heads: Vec<Result<Head, String>> =
            vec![Err("head not fetched".to_string()); self.chains.len()];
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((i, head)) => heads[i] = head.map_err(|e| format!("{e:#}")),
                Err(e) => tracing::error!("head task failed: {e}"),
            }
        }
        Heads(heads)
    }

    /// The clock used for expiry checks: the newest chain block time (so a
    /// lagging chain can never extend a subscription) or the wall clock.
    pub fn now(&self, heads: &Heads) -> u64 {
        let wall = crate::unix_now();
        if self.use_chain_time {
            heads.newest_timestamp().unwrap_or(wall)
        } else {
            wall
        }
    }

    /// Scan every chain and token for payments to `address`.
    pub async fn scan(&self, address: &str, heads: &Heads) -> Scan {
        let address = address.to_ascii_lowercase();
        let mut chains = Vec::with_capacity(self.chains.len());
        let mut payments = Vec::new();
        for (ci, client) in self.chains.iter().enumerate() {
            let head = heads.0.get(ci).cloned().unwrap_or(Err("no head".into()));
            let mut report = ChainReport {
                name: client.cfg.name.clone(),
                tokens: Vec::with_capacity(client.cfg.tokens.len()),
                error: head.as_ref().err().map(|e| format!("head: {e}")),
            };
            for (ti, token) in client.cfg.tokens.iter().enumerate() {
                let key = CursorKey {
                    chain: ci,
                    token: ti,
                    address: address.clone(),
                };
                let cursor = match &head {
                    Ok(head) => match self.advance(client, token, &key, *head).await {
                        Ok(c) => c,
                        Err(e) => {
                            report
                                .error
                                .get_or_insert(format!("{}: {e:#}", token.symbol));
                            self.cached(&key)
                        }
                    },
                    Err(_) => self.cached(&key),
                };
                let mut paid = 0u128;
                let mut paid_secs = 0u64;
                for p in &cursor.payments {
                    let secs = duration_secs(p.amount, token.rate_per_minute);
                    paid = paid.saturating_add(p.amount);
                    paid_secs = paid_secs.saturating_add(secs);
                    payments.push((p.timestamp, secs));
                }
                report.tokens.push(TokenReport {
                    symbol: token.symbol.clone(),
                    decimals: token.decimals,
                    transfers: cursor.payments.len(),
                    paid,
                    paid_secs,
                });
            }
            chains.push(report);
        }
        Scan {
            subscription: Subscription::from_payments(payments, self.now(heads)),
            chains,
        }
    }

    fn cached(&self, key: &CursorKey) -> Cursor {
        self.cache
            .lock()
            .expect("ledger cache poisoned")
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Extend the cursor for `key` up to the confirmed head.
    async fn advance(
        &self,
        client: &ChainClient,
        token: &Token,
        key: &CursorKey,
        head: Head,
    ) -> Result<Cursor> {
        let mut cursor = self.cached(key);
        cursor.next_block = cursor.next_block.max(client.cfg.from_block);
        let Some(target) = head.number.checked_sub(client.cfg.confirmations) else {
            return Ok(cursor);
        };
        if cursor.next_block > target {
            return Ok(cursor);
        }
        let transfers = client
            .eth
            .transfers(
                &token.address,
                &key.address,
                cursor.next_block,
                target,
                client.cfg.log_range,
            )
            .await?;
        let mut block_times: HashMap<u64, u64> = HashMap::new();
        for t in transfers {
            let timestamp = match t.timestamp {
                Some(ts) => ts,
                None => {
                    if let Some(&ts) = block_times.get(&t.block) {
                        ts
                    } else {
                        let ts = client.eth.block_timestamp(t.block).await?;
                        block_times.insert(t.block, ts);
                        ts
                    }
                }
            };
            cursor.payments.push(Payment {
                timestamp,
                amount: t.amount,
            });
        }
        cursor.next_block = target + 1;

        let mut cache = self.cache.lock().expect("ledger cache poisoned");
        let entry = cache.entry(key.clone()).or_default();
        // A concurrent scan may have advanced further; never move backwards.
        if cursor.next_block > entry.next_block {
            *entry = cursor.clone();
        }
        Ok(entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEI: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn duration_follows_rate() {
        assert_eq!(duration_secs(50 * WEI, WEI), 3000);
        assert_eq!(duration_secs(20 * WEI, 2 * WEI), 600);
        assert_eq!(duration_secs(WEI / 2, WEI), 30);
        assert_eq!(duration_secs(1, WEI), 0);
        assert_eq!(duration_secs(u128::MAX, 1), u64::MAX);
    }

    #[test]
    fn no_payments_is_inactive() {
        let s = Subscription::from_payments(vec![], 1000);
        assert!(!s.is_active());
        assert_eq!((s.expiry, s.paid_secs, s.remaining_secs()), (None, 0, 0));
    }

    #[test]
    fn payments_while_active_extend_the_period() {
        let s = Subscription::from_payments(vec![(1000, 600), (1300, 600)], 1500);
        assert_eq!(s.expiry, Some(2200));
        assert_eq!(s.paid_secs, 1200);
        assert!(s.is_active());
        assert_eq!(s.remaining_secs(), 700);
    }

    #[test]
    fn payment_after_lapse_starts_a_new_period() {
        // The old "first payment + total" model would expire this at 2200.
        let s = Subscription::from_payments(vec![(5000, 600), (1000, 600)], 5500);
        assert_eq!(s.expiry, Some(5600));
        assert!(s.is_active());
        let later = Subscription::from_payments(vec![(1000, 600), (5000, 600)], 5600);
        assert!(!later.is_active(), "expiry is exclusive");
    }

    #[test]
    fn dust_payments_do_not_open_a_period() {
        let s = Subscription::from_payments(vec![(1000, 0)], 1000);
        assert_eq!(s.expiry, None);
        assert!(!s.is_active());
    }
}
