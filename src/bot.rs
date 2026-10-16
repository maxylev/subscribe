//! Bot logic: Telegram commands, reply rendering and job scheduling.
//!
//! [`Bot`] is shared (`Arc`) between the update handlers and the scheduler.
//! Job execution is bounded by a semaphore (`max_concurrent_jobs`) and a
//! user's job never runs twice at the same time. Scheduling uses the wall
//! clock; subscription expiry uses the ledger clock (chain time by default).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::fmt::{self, html};
use crate::jobs::{self, Action, Job, Runner};
use crate::ledger::{Ledger, Scan};
use crate::store::{Store, UserRecord};
use crate::telegram::{self, Button, CallbackQuery, Message, TgClient, Update};

const KEYBOARD: &[&[Button]] = &[
    &[("📊 Status", "status"), ("▶️ Run job", "run")],
    &[("⚙️ Set job", "config"), ("❓ Help", "help")],
];

const HELP: &str = "<b>Commands</b>
/start — your payment address and prices
/status — subscription state and payments
/config &lt;json&gt; — set your job (send /config alone for examples)
/run — run your job now
/help — this message";

const CONFIG_HELP: &str = r#"⚙️ <b>Job config</b>
Send <code>/config {json}</code>. Placeholders like <code>{keyword}</code> are filled from <code>input</code>.

HTTP job:
<pre>{"name":"search","kind":"http","interval_secs":300,"input":{"keyword":"rust"},"http":{"url":"https://example.com/?q={keyword}"}}</pre>
Shell job:
<pre>{"name":"echo","kind":"shell","interval_secs":600,"input":{"x":"hi"},"shell":{"cmd":"/bin/echo","args":["{x}"]}}</pre>
Optional: <code>timeout_secs</code>; http <code>method</code>, <code>headers</code>, <code>body</code>."#;

pub struct Bot {
    cfg: Config,
    store: Store,
    ledger: Ledger,
    runner: Runner,
    tg: TgClient,
    permits: Arc<Semaphore>,
    /// Wall-clock time of each user's last job start.
    last_run: Mutex<HashMap<u32, u64>>,
    /// Users whose job is executing right now.
    running: Mutex<HashSet<u32>>,
}

/// Removes a user from `running` when the job finishes, even on panic.
struct RunningGuard<'a> {
    bot: &'a Bot,
    index: u32,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.bot
            .running
            .lock()
            .expect("running set poisoned")
            .remove(&self.index);
    }
}

impl Bot {
    pub fn new(cfg: Config, store: Store, ledger: Ledger, tg: TgClient) -> Result<Self> {
        let runner = Runner::new(cfg.job_timeout_secs, cfg.job_max_output)?;
        Ok(Self {
            permits: Arc::new(Semaphore::new(cfg.max_concurrent_jobs)),
            cfg,
            store,
            ledger,
            runner,
            tg,
            last_run: Mutex::new(HashMap::new()),
            running: Mutex::new(HashSet::new()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn telegram(&self) -> &TgClient {
        &self.tg
    }

    /// Wait until no job is executing (used for graceful shutdown).
    pub async fn drain(&self) {
        let n = u32::try_from(self.cfg.max_concurrent_jobs).unwrap_or(u32::MAX);
        let _all = self.permits.acquire_many(n).await;
    }

    /// Handle one Telegram update end to end; errors become replies.
    pub async fn handle_update(&self, update: Update) {
        let (chat_id, user_id, reply) = if let Some(cq) = update.callback_query {
            if let Err(e) = self.tg.answer_callback_query(&cq.id).await {
                tracing::warn!("answerCallbackQuery: {e:#}");
            }
            let (chat_id, user_id) = callback_target(&cq);
            let reply = self.on_button(cq.data.as_deref().unwrap_or_default(), user_id);
            (chat_id, user_id, reply.await)
        } else if let Some(msg) = update.message {
            let Some((chat_id, user_id)) = message_target(&msg) else {
                return;
            };
            let reply = self.on_text(msg.text.as_deref().unwrap_or_default(), user_id);
            (chat_id, user_id, reply.await)
        } else {
            return;
        };
        let html = reply.unwrap_or_else(|e| format!("⚠️ {}", html(&format!("{e:#}"))));
        if let Err(e) = self.tg.send_html(chat_id, &html, KEYBOARD).await {
            tracing::warn!(user_id, "sendMessage: {e:#}");
        }
    }

    async fn on_text(&self, text: &str, user_id: i64) -> Result<String> {
        match telegram::parse_command(text) {
            Some(("/start", _)) => self.start(user_id),
            Some(("/status", _)) => self.status(user_id).await,
            Some(("/config", arg)) => self.set_job(user_id, arg),
            Some(("/run", _)) => self.run_now(user_id).await,
            _ => Ok(HELP.to_string()),
        }
    }

    async fn on_button(&self, data: &str, user_id: i64) -> Result<String> {
        match data {
            "status" => self.status(user_id).await,
            "run" => self.run_now(user_id).await,
            "config" => Ok(CONFIG_HELP.to_string()),
            _ => Ok(HELP.to_string()),
        }
    }

    fn start(&self, user_id: i64) -> Result<String> {
        let (index, _) = self.store.ensure(user_id)?;
        let address = self.store.address(index)?;
        let mut out = format!(
            "👋 <b>Welcome!</b>\nYour payment address (#{index}):\n<code>{address}</code>\n\n\
             Send any accepted token on any listed chain to start or extend your subscription:\n"
        );
        for chain in &self.cfg.chains {
            for t in &chain.tokens {
                let _ = writeln!(
                    out,
                    "• {}: <b>{}</b> — {}",
                    html(&chain.name),
                    html(&t.symbol),
                    html(&t.price())
                );
            }
        }
        out.push_str("\nThen set a job with /config and check /status.");
        Ok(out)
    }

    fn set_job(&self, user_id: i64, arg: &str) -> Result<String> {
        if arg.is_empty() {
            return Ok(CONFIG_HELP.to_string());
        }
        let job = jobs::parse(arg)?;
        let (index, _) = self.store.ensure(user_id)?;
        let summary = job_summary(&job);
        self.store.set_job(index, Some(job))?;
        // Run the new job on the next tick.
        self.last_run
            .lock()
            .expect("last_run poisoned")
            .remove(&index);
        Ok(format!("✅ Job saved: {summary}"))
    }

    async fn status(&self, user_id: i64) -> Result<String> {
        let (index, rec) = self.store.ensure(user_id)?;
        let address = self.store.address(index)?;
        let heads = self.ledger.heads().await;
        let scan = self.ledger.scan(&address, &heads).await;
        Ok(render_status(index, &address, &rec, &scan))
    }

    /// Manual trigger: runs the job now if the subscription is active.
    async fn run_now(&self, user_id: i64) -> Result<String> {
        let (index, rec) = self.store.ensure(user_id)?;
        let job = rec.job.context("no job configured — use /config")?;
        let address = self.store.address(index)?;
        let heads = self.ledger.heads().await;
        if !self
            .ledger
            .scan(&address, &heads)
            .await
            .subscription
            .is_active()
        {
            bail!("subscription is not active — pay to {address} first (see /start)");
        }
        self.execute(index, &job).await
    }

    /// Run `job` for user `index` under the concurrency limits.
    async fn execute(&self, index: u32, job: &Job) -> Result<String> {
        if !self
            .running
            .lock()
            .expect("running set poisoned")
            .insert(index)
        {
            bail!("job {} is already running", job.name());
        }
        let _guard = RunningGuard { bot: self, index };
        let _permit = self.permits.acquire().await.context("shutting down")?;
        self.last_run
            .lock()
            .expect("last_run poisoned")
            .insert(index, crate::unix_now());
        let started = Instant::now();
        let name = html(job.name());
        Ok(match self.runner.run(job).await {
            Ok(output) => format!(
                "✅ <b>{name}</b> finished in {:.1}s\n<pre>{}</pre>",
                started.elapsed().as_secs_f64(),
                html(&output)
            ),
            Err(e) => format!("❌ <b>{name}</b> failed: {}", html(&format!("{e:#}"))),
        })
    }

    /// Scheduler tick: run every due job whose subscription is active.
    pub async fn tick(self: &Arc<Self>) {
        let users = match self.store.users() {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("listing users: {e:#}");
                return;
            }
        };
        let now = crate::unix_now();
        let due: Vec<(u32, UserRecord, Job)> = {
            let last_run = self.last_run.lock().expect("last_run poisoned");
            let running = self.running.lock().expect("running set poisoned");
            users
                .into_iter()
                .filter_map(|(i, rec)| {
                    let job = rec.job.clone()?;
                    let is_due = last_run
                        .get(&i)
                        .is_none_or(|&t| now.saturating_sub(t) >= job.interval_secs);
                    (is_due && !running.contains(&i)).then_some((i, rec, job))
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        let heads = self.ledger.heads().await;
        for (index, rec, job) in due {
            let address = match self.store.address(index) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(index, "address: {e:#}");
                    continue;
                }
            };
            let sub = self.ledger.scan(&address, &heads).await.subscription;
            tracing::debug!(
                index,
                active = sub.is_active(),
                remaining = sub.remaining_secs(),
                "tick"
            );
            if !sub.is_active() {
                continue;
            }
            let bot = Arc::clone(self);
            tokio::spawn(async move {
                let report = match bot.execute(index, &job).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!(index, "skipped: {e:#}");
                        return;
                    }
                };
                tracing::info!(index, job = job.name(), "job finished");
                if let Err(e) = bot.tg.send_html(rec.telegram_id, &report, &[]).await {
                    tracing::warn!(index, "delivering report: {e:#}");
                }
            });
        }
    }
}

/// `(chat, user)` of a message; messages from bots are ignored.
fn message_target(msg: &Message) -> Option<(i64, i64)> {
    match &msg.from {
        Some(u) if u.is_bot => None,
        Some(u) => Some((msg.chat.id, u.id)),
        None => None,
    }
}

fn callback_target(cq: &CallbackQuery) -> (i64, i64) {
    let chat = cq.message.as_ref().map_or(cq.from.id, |m| m.chat.id);
    (chat, cq.from.id)
}

fn job_summary(job: &Job) -> String {
    let kind = match job.action {
        Action::Http { .. } => "http",
        Action::Shell { .. } => "shell",
    };
    format!(
        "<code>{}</code> ({kind}, every {})",
        html(job.name()),
        fmt::duration(job.interval_secs)
    )
}

/// HTML `/status` reply.
pub fn render_status(index: u32, address: &str, rec: &UserRecord, scan: &Scan) -> String {
    let sub = &scan.subscription;
    let state = match sub.expiry {
        None => "❌ no payments yet".to_string(),
        Some(e) if sub.is_active() => format!(
            "✅ active — {} left (until {})",
            fmt::duration(sub.remaining_secs()),
            fmt::utc(e)
        ),
        Some(e) => format!("❌ expired {}", fmt::relative(e, sub.now)),
    };
    let job = rec
        .job
        .as_ref()
        .map_or_else(|| "none — use /config".to_string(), job_summary);
    let mut out = format!(
        "📊 <b>Subscription #{index}</b>\nAddress: <code>{address}</code>\nStatus: {state}\n\
         Paid time: {}\nJob: {job}\n",
        fmt::duration(sub.paid_secs)
    );
    let mut lines = String::new();
    for chain in &scan.chains {
        for t in chain.tokens.iter().filter(|t| t.transfers > 0) {
            let _ = writeln!(
                lines,
                "• {} {}: {} ({}, {} transfer{})",
                html(&chain.name),
                html(&t.symbol),
                fmt::amount(t.paid, t.decimals),
                fmt::duration(t.paid_secs),
                t.transfers,
                if t.transfers == 1 { "" } else { "s" }
            );
        }
        if let Some(e) = &chain.error {
            let _ = writeln!(lines, "⚠️ {}: {}", html(&chain.name), html(e));
        }
    }
    if !lines.is_empty() {
        out.push_str("Payments:\n");
        out.push_str(&lines);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{ChainReport, Subscription, TokenReport};

    #[test]
    fn status_rendering() {
        let rec = UserRecord {
            telegram_id: 1,
            created_at: 0,
            job: Some(
                jobs::parse(
                    r#"{"name":"ab","kind":"shell","interval_secs":60,"shell":{"cmd":"/bin/true"}}"#,
                )
                .unwrap(),
            ),
        };
        let scan = Scan {
            subscription: Subscription::from_payments(vec![(1000, 3600)], 1600),
            chains: vec![ChainReport {
                name: "anvil<1>".to_string(),
                tokens: vec![TokenReport {
                    symbol: "SUB".to_string(),
                    decimals: 18,
                    transfers: 1,
                    paid: 60_000_000_000_000_000_000,
                    paid_secs: 3600,
                }],
                error: Some("head: down & out".to_string()),
            }],
        };
        let out = render_status(0, "0xAbC", &rec, &scan);
        assert!(out.contains("✅ active — 50m left"), "{out}");
        assert!(out.contains("Paid time: 1h 00m"), "{out}");
        assert!(
            out.contains("• anvil&lt;1&gt; SUB: 60 (1h 00m, 1 transfer)"),
            "{out}"
        );
        assert!(
            out.contains("⚠️ anvil&lt;1&gt;: head: down &amp; out"),
            "{out}"
        );
        assert!(out.contains("<code>ab</code> (shell, every 1m)"), "{out}");

        let empty = Scan {
            subscription: Subscription::from_payments(vec![], 0),
            chains: vec![],
        };
        let out = render_status(
            3,
            "0x0",
            &UserRecord {
                telegram_id: 1,
                created_at: 0,
                job: None,
            },
            &empty,
        );
        assert!(
            out.contains("❌ no payments yet") && !out.contains("Payments:"),
            "{out}"
        );
    }
}
