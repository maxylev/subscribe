//! `subscribe` command-line interface: the server plus operational commands.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use subscribe::bip32::{DEFAULT_ACCOUNT_PATH, Xprv};
use subscribe::config::Config;
use subscribe::ledger::Ledger;
use subscribe::store::Store;
use subscribe::{fmt, server};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct ConfigArg {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "SUBSCRIBE_CONFIG", default_value = "config.toml")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Run the subscription server (Telegram bot + scheduler).
    Serve(ConfigArg),
    /// Derive the watch-only account xpub to put in the config.
    ///
    /// Run this offline. Without --seed-hex a random 32-byte seed is generated
    /// and printed to stderr: back it up, it controls all user funds.
    Genxpub {
        /// BIP-32 seed as hex (16-64 bytes).
        #[arg(long)]
        seed_hex: Option<String>,
        /// Account derivation path.
        #[arg(long, default_value = DEFAULT_ACCOUNT_PATH)]
        path: String,
    },
    /// Print the payment address of a user index.
    Addr {
        index: u32,
        #[command(flatten)]
        config: ConfigArg,
    },
    /// List users with their subscription state.
    Users(ConfigArg),
    /// Show the detailed subscription state of a user.
    Check {
        index: u32,
        #[command(flatten)]
        config: ConfigArg,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        // No color codes in docker logs / journald.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    match Cli::parse().command {
        Command::Serve(c) => server::serve(Config::load(&c.config)?).await,
        Command::Genxpub { seed_hex, path } => genxpub(seed_hex.as_deref(), &path),
        Command::Addr { index, config } => {
            let (_, store) = open(&config)?;
            println!("{}", store.address(index)?);
            Ok(())
        }
        Command::Users(config) => users(&config).await,
        Command::Check { index, config } => check(index, &config).await,
    }
}

fn open(c: &ConfigArg) -> Result<(Config, Store)> {
    let cfg = Config::load(&c.config)?;
    let store = Store::open(&cfg.data_dir, cfg.xpub.clone())?;
    Ok((cfg, store))
}

fn genxpub(seed_hex: Option<&str>, path: &str) -> Result<()> {
    let seed = if let Some(h) = seed_hex {
        hex::decode(h.trim_start_matches("0x")).context("--seed-hex is not valid hex")?
    } else {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf)
            .map_err(|e| anyhow::anyhow!("OS random number generator failed: {e}"))?;
        eprintln!(
            "generated seed (back it up, keep it offline): {}",
            hex::encode(buf)
        );
        buf.to_vec()
    };
    let account = Xprv::from_seed(&seed)?.derive_path(path)?;
    println!("{}", account.to_xpub());
    Ok(())
}

async fn users(c: &ConfigArg) -> Result<()> {
    let (cfg, store) = open(c)?;
    let ledger = Ledger::new(&cfg.chains, cfg.use_chain_time)?;
    let heads = ledger.heads().await;
    println!("index\ttelegram_id\taddress\tactive\tremaining\tjob");
    for (index, rec) in store.users()? {
        let address = store.address(index)?;
        let sub = ledger.scan(&address, &heads).await.subscription;
        println!(
            "{index}\t{}\t{address}\t{}\t{}\t{}",
            rec.telegram_id,
            sub.is_active(),
            fmt::duration(sub.remaining_secs()),
            rec.job.as_ref().map_or("-", |j| j.name())
        );
    }
    Ok(())
}

async fn check(index: u32, c: &ConfigArg) -> Result<()> {
    let (cfg, store) = open(c)?;
    let rec = store
        .get(index)?
        .with_context(|| format!("no user at index {index}"))?;
    let ledger = Ledger::new(&cfg.chains, cfg.use_chain_time)?;
    let address = store.address(index)?;
    let scan = ledger.scan(&address, &ledger.heads().await).await;
    let sub = &scan.subscription;
    println!("index: {index}");
    println!("address: {address}");
    println!("telegram_id: {}", rec.telegram_id);
    println!("active: {}", sub.is_active());
    println!("paid_secs: {}", sub.paid_secs);
    println!("remaining_secs: {}", sub.remaining_secs());
    match sub.expiry {
        Some(e) => println!("expiry: {e} ({})", fmt::utc(e)),
        None => println!("expiry: none"),
    }
    println!("now: {} ({})", sub.now, fmt::utc(sub.now));
    println!("job: {}", rec.job.as_ref().map_or("-", |j| j.name()));
    println!("payments:");
    for chain in &scan.chains {
        for t in &chain.tokens {
            println!(
                "  {}/{}: {} raw={} transfers={} secs={}",
                chain.name,
                t.symbol,
                fmt::amount(t.paid, t.decimals),
                t.paid,
                t.transfers,
                t.paid_secs
            );
        }
        if let Some(e) = &chain.error {
            println!("  {}: ERROR {e}", chain.name);
        }
    }
    Ok(())
}
