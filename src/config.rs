//! Configuration loading and validation.
//!
//! The server is configured through a TOML file (`[server]`, `[[chain]]`,
//! `[[chain.token]]`) plus `SUBSCRIBE_*` environment overrides for
//! server-level settings — most notably `SUBSCRIBE_BOT_TOKEN`, which should
//! never be committed to a file. Unknown keys are rejected so typos fail
//! loudly instead of silently falling back to defaults.
//!
//! Pricing is per token: `rate_per_minute` is the amount of the token's
//! smallest unit that buys one minute of subscription time.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::bip32::Xpub;

/// Telegram Bot API endpoint used when `tg_base_url` is not set.
pub const TELEGRAM_API: &str = "https://api.telegram.org";

/// A string that never appears in `Debug` output or logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Fully validated server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory of user files (`{index}.json`).
    pub data_dir: PathBuf,
    /// Interval of the payment rescan / job scheduling loop.
    pub cron_interval_secs: u64,
    /// Telegram `getUpdates` long-poll timeout.
    pub poll_secs: u64,
    /// Default and maximum job timeout; jobs may only ask for less.
    pub job_timeout_secs: u64,
    /// Maximum job output bytes forwarded to Telegram.
    pub job_max_output: usize,
    /// Maximum number of jobs executing at the same time.
    pub max_concurrent_jobs: usize,
    /// Use the newest chain block time as "now" instead of the wall clock.
    pub use_chain_time: bool,
    pub bot_token: Option<Secret>,
    pub tg_base_url: String,
    /// Watch-only account key (`m/44'/60'/0'/0`); user `i` pays to child `i`.
    pub xpub: Xpub,
    /// Enabled chains only, in configuration order.
    pub chains: Vec<Chain>,
}

/// One EVM chain and the tokens accepted on it.
#[derive(Debug, Clone)]
pub struct Chain {
    /// Display name, unique across the configuration.
    pub name: String,
    pub rpc_url: String,
    /// First block scanned for payments.
    pub from_block: u64,
    /// Maximum block span of a single `eth_getLogs` request.
    pub log_range: u64,
    /// Blocks behind the head that are considered final.
    pub confirmations: u64,
    /// Enabled tokens only.
    pub tokens: Vec<Token>,
}

/// An ERC-20 token accepted as payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Lowercase 0x-prefixed contract address.
    pub address: String,
    pub symbol: String,
    pub decimals: u8,
    /// Smallest token units granting one minute of subscription.
    pub rate_per_minute: u128,
}

impl Token {
    /// Price as token units per minute, e.g. `1.5 USDC/min`.
    pub fn price(&self) -> String {
        format!(
            "{} {}/min",
            crate::fmt::amount(self.rate_per_minute, self.decimals),
            self.symbol
        )
    }
}

impl Config {
    /// Load a TOML file and apply `SUBSCRIBE_*` overrides from the process
    /// environment.
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_env(path, |key| std::env::var(key).ok())
    }

    /// Like [`Config::load`] with an injectable environment lookup.
    pub fn load_with_env(path: &Path, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        Self::from_raw(raw, env)
    }

    fn from_raw(mut raw: RawConfig, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        raw.server.apply_env(&env)?;
        let server = raw.server;

        ensure!(
            server.cron_interval_secs > 0,
            "cron_interval_secs must be > 0"
        );
        ensure!(server.poll_secs > 0, "poll_secs must be > 0");
        ensure!(server.job_timeout_secs > 0, "job_timeout_secs must be > 0");
        ensure!(server.job_max_output > 0, "job_max_output must be > 0");
        ensure!(
            server.max_concurrent_jobs > 0,
            "max_concurrent_jobs must be > 0"
        );
        ensure!(server.log_range > 0, "log_range must be > 0");

        let xpub_str = server
            .xpub
            .as_deref()
            .context("[server] xpub is required (or SUBSCRIBE_XPUB)")?;
        let xpub = Xpub::from_str(xpub_str).context("invalid [server] xpub")?;

        let mut names = HashSet::new();
        let mut chains = Vec::new();
        for rc in raw.chain {
            ensure!(
                names.insert(rc.name.clone()),
                "duplicate chain name {:?}",
                rc.name
            );
            if rc.enabled {
                chains.push(rc.resolve(&server)?);
            }
        }
        ensure!(!chains.is_empty(), "no enabled [[chain]] configured");

        Ok(Self {
            data_dir: server.data_dir,
            cron_interval_secs: server.cron_interval_secs,
            poll_secs: server.poll_secs,
            job_timeout_secs: server.job_timeout_secs,
            job_max_output: server.job_max_output,
            max_concurrent_jobs: server.max_concurrent_jobs,
            use_chain_time: server.use_chain_time,
            bot_token: server.bot_token.filter(|t| !t.is_empty()).map(Secret),
            tg_base_url: server
                .tg_base_url
                .unwrap_or_else(|| TELEGRAM_API.to_string())
                .trim_end_matches('/')
                .to_string(),
            xpub,
            chains,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    chain: Vec<RawChain>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawServer {
    data_dir: PathBuf,
    cron_interval_secs: u64,
    poll_secs: u64,
    job_timeout_secs: u64,
    job_max_output: usize,
    max_concurrent_jobs: usize,
    use_chain_time: bool,
    bot_token: Option<String>,
    tg_base_url: Option<String>,
    xpub: Option<String>,
    /// Defaults for every chain that does not override them.
    from_block: u64,
    log_range: u64,
    confirmations: u64,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            cron_interval_secs: 60,
            poll_secs: 25,
            job_timeout_secs: 15,
            job_max_output: 3500,
            max_concurrent_jobs: 4,
            use_chain_time: true,
            bot_token: None,
            tg_base_url: None,
            xpub: None,
            from_block: 0,
            log_range: 10_000,
            confirmations: 0,
        }
    }
}

impl RawServer {
    fn apply_env(&mut self, env: &impl Fn(&str) -> Option<String>) -> Result<()> {
        fn parse<T: FromStr>(key: &str, value: &str) -> Result<T> {
            value
                .parse()
                .map_err(|_| anyhow::anyhow!("{key}={value:?} is not a valid value"))
        }
        let get = |key: &str| env(key).filter(|v| !v.is_empty());
        if let Some(v) = get("SUBSCRIBE_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("SUBSCRIBE_CRON_INTERVAL_SECS") {
            self.cron_interval_secs = parse("SUBSCRIBE_CRON_INTERVAL_SECS", &v)?;
        }
        if let Some(v) = get("SUBSCRIBE_POLL_SECS") {
            self.poll_secs = parse("SUBSCRIBE_POLL_SECS", &v)?;
        }
        if let Some(v) = get("SUBSCRIBE_JOB_TIMEOUT_SECS") {
            self.job_timeout_secs = parse("SUBSCRIBE_JOB_TIMEOUT_SECS", &v)?;
        }
        if let Some(v) = get("SUBSCRIBE_JOB_MAX_OUTPUT") {
            self.job_max_output = parse("SUBSCRIBE_JOB_MAX_OUTPUT", &v)?;
        }
        if let Some(v) = get("SUBSCRIBE_MAX_CONCURRENT_JOBS") {
            self.max_concurrent_jobs = parse("SUBSCRIBE_MAX_CONCURRENT_JOBS", &v)?;
        }
        if let Some(v) = get("SUBSCRIBE_USE_CHAIN_TIME") {
            self.use_chain_time = parse("SUBSCRIBE_USE_CHAIN_TIME", &v)?;
        }
        if let Some(v) = get("SUBSCRIBE_BOT_TOKEN") {
            self.bot_token = Some(v);
        }
        if let Some(v) = get("SUBSCRIBE_TG_BASE_URL") {
            self.tg_base_url = Some(v);
        }
        if let Some(v) = get("SUBSCRIBE_XPUB") {
            self.xpub = Some(v);
        }
        if let Some(v) = get("SUBSCRIBE_FROM_BLOCK") {
            self.from_block = parse("SUBSCRIBE_FROM_BLOCK", &v)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChain {
    name: String,
    rpc_url: String,
    from_block: Option<u64>,
    log_range: Option<u64>,
    confirmations: Option<u64>,
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default)]
    token: Vec<RawToken>,
}

impl RawChain {
    fn resolve(self, server: &RawServer) -> Result<Chain> {
        let name = self.name;
        ensure!(!name.trim().is_empty(), "chain name must not be empty");
        ensure!(
            self.rpc_url.starts_with("http://") || self.rpc_url.starts_with("https://"),
            "chain {name}: rpc_url must be an http(s) URL"
        );
        let log_range = self.log_range.unwrap_or(server.log_range);
        ensure!(log_range > 0, "chain {name}: log_range must be > 0");

        let mut addresses = HashSet::new();
        let mut tokens = Vec::new();
        for rt in self.token.into_iter().filter(|t| t.enabled) {
            let token = rt
                .resolve()
                .with_context(|| format!("chain {name}: invalid token"))?;
            ensure!(
                addresses.insert(token.address.clone()),
                "chain {name}: duplicate token {}",
                token.address
            );
            tokens.push(token);
        }
        ensure!(!tokens.is_empty(), "chain {name}: no enabled tokens");

        Ok(Chain {
            rpc_url: self.rpc_url,
            from_block: self.from_block.unwrap_or(server.from_block),
            log_range,
            confirmations: self.confirmations.unwrap_or(server.confirmations),
            tokens,
            name,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToken {
    address: String,
    symbol: String,
    decimals: u8,
    rate_per_minute: Amount,
    #[serde(default = "enabled")]
    enabled: bool,
}

impl RawToken {
    fn resolve(self) -> Result<Token> {
        let address = self.address.to_ascii_lowercase();
        let valid = address
            .strip_prefix("0x")
            .is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        ensure!(
            valid,
            "{}: address must be 0x + 40 hex digits",
            self.address
        );
        ensure!(
            !self.symbol.is_empty(),
            "{address}: symbol must not be empty"
        );
        // 10^38 is the largest power of ten that fits in u128.
        ensure!(
            self.decimals <= 38,
            "{}: decimals must be <= 38",
            self.symbol
        );
        let rate_per_minute = self.rate_per_minute.0;
        if rate_per_minute == 0 {
            bail!("{}: rate_per_minute must be > 0", self.symbol);
        }
        Ok(Token {
            address,
            symbol: self.symbol,
            decimals: self.decimals,
            rate_per_minute,
        })
    }
}

/// A `u128` accepted as a TOML integer or, for values beyond `i64`, a string.
#[derive(Deserialize)]
#[serde(try_from = "AmountRepr")]
struct Amount(u128);

#[derive(Deserialize)]
#[serde(untagged)]
enum AmountRepr {
    Int(i64),
    Str(String),
}

impl TryFrom<AmountRepr> for Amount {
    type Error = String;

    fn try_from(v: AmountRepr) -> Result<Self, String> {
        match v {
            AmountRepr::Int(i) => u128::try_from(i)
                .map(Amount)
                .map_err(|_| format!("amount must not be negative: {i}")),
            AmountRepr::Str(s) => s
                .trim()
                .replace('_', "")
                .parse()
                .map(Amount)
                .map_err(|_| format!("amount {s:?} is not an unsigned integer")),
        }
    }
}

const fn enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const XPUB: &str = "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8";

    fn load(toml: &str, env: &[(&str, &str)]) -> Result<Config> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let raw: RawConfig = toml::from_str(toml)?;
        Config::from_raw(raw, |k| env.get(k).cloned())
    }

    fn one_chain(token: &str) -> String {
        format!(
            "[server]\nxpub = \"{XPUB}\"\n\n[[chain]]\nname = \"a\"\nrpc_url = \"http://127.0.0.1:8545\"\n\n[[chain.token]]\n{token}\n"
        )
    }

    const SUB: &str = "address = \"0x1111111111111111111111111111111111111111\"\nsymbol = \"SUB\"\ndecimals = 18\nrate_per_minute = \"1000000000000000000\"";

    #[test]
    fn multi_chain_multi_token() {
        let cfg = load(
            &format!(
                r#"[server]
data_dir = "users"
xpub = "{XPUB}"
log_range = 500

[[chain]]
name = "mainnet"
rpc_url = "https://rpc.example/1"

[[chain.token]]
address = "0xA0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
symbol = "USDC"
decimals = 6
rate_per_minute = 1_000_000

[[chain.token]]
address = "0x2222222222222222222222222222222222222222"
symbol = "USDT"
decimals = 6
rate_per_minute = "2000000"
enabled = false

[[chain]]
name = "arbitrum"
rpc_url = "https://rpc.example/42161"
from_block = 12345
confirmations = 3

[[chain.token]]
address = "0x3333333333333333333333333333333333333333"
symbol = "USDC"
decimals = 6
rate_per_minute = "1_500_000"

[[chain]]
name = "off"
rpc_url = "https://rpc.example/off"
enabled = false
"#
            ),
            &[],
        )
        .unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("users"));
        assert_eq!(cfg.chains.len(), 2, "disabled chain is dropped");
        let main = &cfg.chains[0];
        assert_eq!(
            (main.from_block, main.log_range, main.confirmations),
            (0, 500, 0)
        );
        assert_eq!(main.tokens.len(), 1, "disabled token is dropped");
        assert_eq!(
            main.tokens[0].address,
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert_eq!(main.tokens[0].rate_per_minute, 1_000_000);
        let arb = &cfg.chains[1];
        assert_eq!((arb.from_block, arb.confirmations), (12345, 3));
        assert_eq!(arb.tokens[0].rate_per_minute, 1_500_000);
        assert_eq!(cfg.tg_base_url, TELEGRAM_API);
    }

    #[test]
    fn env_overrides_win_and_reach_chains() {
        let cfg = load(
            &one_chain(SUB),
            &[
                ("SUBSCRIBE_CRON_INTERVAL_SECS", "42"),
                ("SUBSCRIBE_BOT_TOKEN", "env-token"),
                ("SUBSCRIBE_FROM_BLOCK", "777"),
                ("SUBSCRIBE_USE_CHAIN_TIME", "false"),
            ],
        )
        .unwrap();
        assert_eq!(cfg.cron_interval_secs, 42);
        assert_eq!(
            cfg.bot_token.as_ref().map(Secret::expose),
            Some("env-token")
        );
        assert_eq!(cfg.chains[0].from_block, 777);
        assert!(!cfg.use_chain_time);
        assert!(
            !format!("{cfg:?}").contains("env-token"),
            "token is redacted"
        );

        let err = load(&one_chain(SUB), &[("SUBSCRIBE_USE_CHAIN_TIME", "yes")]).unwrap_err();
        assert!(
            err.to_string().contains("SUBSCRIBE_USE_CHAIN_TIME"),
            "{err}"
        );
    }

    #[test]
    fn rejects_invalid_configs() {
        let cases = [
            (format!("[server]\nxpub = \"{XPUB}\"\n"), "no enabled"),
            (one_chain(SUB).replace(XPUB, "not-an-xpub"), "xpub"),
            (
                one_chain(&SUB.replace("\"1000000000000000000\"", "0")),
                "rate_per_minute",
            ),
            (
                one_chain(&SUB.replace("0x1111111111111111111111111111111111111111", "0x11")),
                "address",
            ),
            (
                one_chain(&SUB.replace("decimals = 18", "decimals = 39")),
                "decimals",
            ),
            (one_chain(&format!("{SUB}\ntypo = 1")), "unknown field"),
            (
                one_chain(SUB).replace("http://127.0.0.1:8545", "ws://x"),
                "rpc_url",
            ),
        ];
        for (toml, needle) in cases {
            let err = format!("{:#}", load(&toml, &[]).unwrap_err());
            assert!(err.contains(needle), "expected {needle:?} in {err:?}");
        }
    }

    #[test]
    fn shipped_example_config_is_valid() {
        let raw: RawConfig = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let cfg = Config::from_raw(raw, |_| None).unwrap();
        assert_eq!(cfg.chains.len(), 1, "only the anvil chain is enabled");
        assert_eq!(cfg.chains[0].tokens[0].price(), "1 SUB/min");
        assert_eq!(cfg.job_max_output, RawServer::default().job_max_output);
    }

    #[test]
    fn price_is_human_readable() {
        let token = |rate: u128| Token {
            address: String::new(),
            symbol: "USDC".to_string(),
            decimals: 6,
            rate_per_minute: rate,
        };
        assert_eq!(token(1_000_000).price(), "1 USDC/min");
        assert_eq!(token(500_000).price(), "0.5 USDC/min");
        assert_eq!(token(1_333_333).price(), "1.333333 USDC/min");
    }
}
