//! Minimal JSON-RPC client for EVM chains.
//!
//! Only the calls the ledger needs: chain id, the chain head, block
//! timestamps and ERC-20 `Transfer` logs filtered by recipient. Log queries
//! are split into windows of at most `log_range` blocks because most public
//! RPC providers cap the `eth_getLogs` range.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// Keccak-256 of `Transfer(address,address,uint256)`.
pub const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

const RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// The newest block of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub number: u64,
    pub timestamp: u64,
}

/// One ERC-20 transfer observed in a `Transfer` log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub block: u64,
    /// Block timestamp when the node includes `blockTimestamp` in logs.
    pub timestamp: Option<u64>,
    /// Transferred amount, saturated at `u128::MAX`.
    pub amount: u128,
}

/// JSON-RPC client bound to one endpoint.
#[derive(Clone)]
pub struct Eth {
    client: reqwest::Client,
    url: String,
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcLog {
    data: String,
    block_number: String,
    #[serde(default)]
    block_timestamp: Option<String>,
    #[serde(default)]
    removed: bool,
}

#[derive(Deserialize)]
struct RpcBlock {
    number: String,
    timestamp: String,
}

impl Eth {
    pub fn new(url: &str) -> Result<Self> {
        let client = crate::http_client().timeout(RPC_TIMEOUT).build()?;
        Ok(Self {
            client,
            url: url.to_string(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            // RPC URLs often embed provider API keys: never surface them.
            .map_err(|e| anyhow::anyhow!("{method}: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{method}: http {status}");
        }
        let resp: RpcResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("{method}: invalid response: {}", e.without_url()))?;
        if let Some(err) = resp.error {
            bail!("{method}: rpc error {err}");
        }
        resp.result
            .filter(|v| !v.is_null())
            .with_context(|| format!("{method}: empty result"))
    }

    pub async fn chain_id(&self) -> Result<u64> {
        let v = self.call("eth_chainId", json!([])).await?;
        parse_quantity(v.as_str().context("chainId is not a string")?)
    }

    /// Number and timestamp of the latest block.
    pub async fn head(&self) -> Result<Head> {
        let block = self.block("latest").await?;
        Ok(Head {
            number: parse_quantity(&block.number)?,
            timestamp: parse_quantity(&block.timestamp)?,
        })
    }

    pub async fn block_timestamp(&self, number: u64) -> Result<u64> {
        parse_quantity(&self.block(&format!("0x{number:x}")).await?.timestamp)
    }

    async fn block(&self, tag: &str) -> Result<RpcBlock> {
        let v = self
            .call("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        serde_json::from_value(v).context("malformed block")
    }

    /// All `Transfer`s of `token` to `to` in `[from, to_block]`, oldest first,
    /// queried in windows of at most `log_range` blocks.
    pub async fn transfers(
        &self,
        token: &str,
        to: &str,
        from: u64,
        to_block: u64,
        log_range: u64,
    ) -> Result<Vec<Transfer>> {
        // topic2 is the recipient, left-padded to 32 bytes.
        let recipient = format!(
            "0x{:0>64}",
            to.trim_start_matches("0x").to_ascii_lowercase()
        );
        let mut out = Vec::new();
        let mut start = from;
        while start <= to_block {
            let end = to_block.min(start.saturating_add(log_range.max(1) - 1));
            let params = json!([{
                "fromBlock": format!("0x{start:x}"),
                "toBlock": format!("0x{end:x}"),
                "address": token,
                "topics": [TRANSFER_TOPIC, null, recipient],
            }]);
            let logs: Vec<RpcLog> = serde_json::from_value(self.call("eth_getLogs", params).await?)
                .context("malformed logs")?;
            for log in logs.into_iter().filter(|l| !l.removed) {
                out.push(Transfer {
                    block: parse_quantity(&log.block_number)?,
                    timestamp: log
                        .block_timestamp
                        .as_deref()
                        .map(parse_quantity)
                        .transpose()?,
                    amount: parse_amount(&log.data)?,
                });
            }
            let Some(next) = end.checked_add(1) else {
                break;
            };
            start = next;
        }
        out.sort_by_key(|t| t.block);
        Ok(out)
    }
}

/// Parse a hex `QUANTITY` such as `0x1b4`.
fn parse_quantity(s: &str) -> Result<u64> {
    let digits = s
        .strip_prefix("0x")
        .with_context(|| format!("quantity {s:?} lacks 0x prefix"))?;
    u64::from_str_radix(digits, 16).with_context(|| format!("bad quantity {s:?}"))
}

/// Parse the 32-byte `uint256` log payload, saturating values above `u128::MAX`.
fn parse_amount(data: &str) -> Result<u128> {
    let digits = data.strip_prefix("0x").unwrap_or(data);
    if digits.len() != 64 {
        bail!("transfer data must be 32 bytes, got {data:?}");
    }
    let (high, low) = digits.split_at(32);
    if high.bytes().any(|b| b != b'0') {
        return Ok(u128::MAX);
    }
    u128::from_str_radix(low, 16).with_context(|| format!("bad transfer amount {data:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(parse_quantity("0x0").unwrap(), 0);
        assert_eq!(parse_quantity("0x1b4").unwrap(), 436);
        assert!(parse_quantity("1b4").is_err());
        assert!(parse_quantity("0xzz").is_err());
    }

    #[test]
    fn amounts_saturate_above_u128() {
        let small = format!("0x{:064x}", 50u128 * 10u128.pow(18));
        assert_eq!(parse_amount(&small).unwrap(), 50 * 10u128.pow(18));
        let huge = format!("0x1{}", "0".repeat(63));
        assert_eq!(parse_amount(&huge).unwrap(), u128::MAX);
        assert!(parse_amount("0x01").is_err());
    }

    #[test]
    fn log_shapes_parse() {
        let log: RpcLog = serde_json::from_value(json!({
            "data": format!("0x{:064x}", 7),
            "blockNumber": "0x10",
            "blockTimestamp": "0x64",
            "topics": []
        }))
        .unwrap();
        assert_eq!(log.block_timestamp.as_deref(), Some("0x64"));
        assert!(!log.removed);
    }
}
