//! Live checks against public networks. Ignored by default:
//!
//! ```text
//! cargo test --test live -- --ignored
//! ```
//!
//! Override the endpoint with `LIVE_ARBITRUM_RPC`.

use serde_json::{Value, json};
use subscribe::config::{Chain, Token};
use subscribe::eth::TRANSFER_TOPIC;
use subscribe::ledger::{Ledger, duration_secs};

const USDC_ARBITRUM: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";

fn rpc_url() -> String {
    std::env::var("LIVE_ARBITRUM_RPC").unwrap_or_else(|_| "https://arb1.arbitrum.io/rpc".into())
}

async fn rpc(method: &str, params: Value) -> Value {
    let resp: Value = subscribe::http_client()
        .build()
        .unwrap()
        .post(rpc_url())
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["result"].clone()
}

fn quantity(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

/// Scan real USDC transfers on Arbitrum and cross-check them against raw logs.
#[tokio::test]
#[ignore = "requires network access"]
async fn ledger_matches_raw_logs_on_arbitrum() {
    let head = quantity(&rpc("eth_blockNumber", json!([])).await);
    let (from, to) = (head - 400, head - 100);
    let logs = rpc(
        "eth_getLogs",
        json!([{
            "fromBlock": format!("0x{from:x}"),
            "toBlock": format!("0x{to:x}"),
            "address": USDC_ARBITRUM,
            "topics": [TRANSFER_TOPIC],
        }]),
    )
    .await;
    let logs = logs.as_array().expect("logs");
    assert!(!logs.is_empty(), "no USDC transfers in 300 Arbitrum blocks");

    // The most frequent recipient in the window.
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for log in logs {
        let topic = log["topics"][2].as_str().unwrap();
        *counts.entry(format!("0x{}", &topic[26..])).or_default() += 1;
    }
    let (recipient, _) = counts.into_iter().max_by_key(|(_, n)| *n).unwrap();
    let expected: Vec<u128> = logs
        .iter()
        .filter(|l| l["topics"][2].as_str().unwrap().ends_with(&recipient[2..]))
        .map(|l| u128::from_str_radix(&l["data"].as_str().unwrap()[2..], 16).unwrap())
        .collect();

    let chain = Chain {
        name: "arbitrum".into(),
        rpc_url: rpc_url(),
        from_block: from,
        log_range: 100, // forces several windows
        confirmations: head - to,
        tokens: vec![Token {
            address: USDC_ARBITRUM.into(),
            symbol: "USDC".into(),
            decimals: 6,
            rate_per_minute: 1_000_000,
        }],
    };
    let ledger = Ledger::new(&[chain], true).unwrap();
    let heads = ledger.heads().await;
    let scan = ledger.scan(&recipient, &heads).await;
    let report = &scan.chains[0];
    assert!(report.error.is_none(), "{:?}", report.error);
    let token = &report.tokens[0];

    // The head may have advanced since `head`, so the ledger can see more.
    assert!(
        token.transfers >= expected.len(),
        "{} < {}",
        token.transfers,
        expected.len()
    );
    assert!(token.paid >= expected.iter().sum::<u128>());
    let secs: u64 = expected.iter().map(|a| duration_secs(*a, 1_000_000)).sum();
    assert!(scan.subscription.paid_secs >= secs);
    if scan.subscription.paid_secs > 0 {
        assert!(scan.subscription.expiry.is_some());
    }
    // Chain time drives the clock and must be close to wall time.
    assert!(scan.subscription.now.abs_diff(subscribe::unix_now()) < 600);
}
