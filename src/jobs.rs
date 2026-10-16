//! User-configurable jobs.
//!
//! Two kinds: `http` (outbound request) and `shell` (subprocess, no shell
//! interpolation — `cmd` is executed directly with `args`). String fields
//! support `{field}` templating from the job's `input` object; values
//! substituted into a URL are percent-encoded. Output is capped at
//! `max_output` bytes while it is read, so a chatty job cannot exhaust
//! memory, and every job runs under a timeout that the server bounds.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Default interval between scheduled runs.
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
const MAX_NAME_LEN: usize = 64;
const HTTP_METHODS: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// Settings shared by every job kind. (`deny_unknown_fields` is applied to
/// the nested specs; serde does not support it together with `flatten`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    /// Seconds between scheduled runs.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Template variables for `{field}` placeholders.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input: BTreeMap<String, Value>,
    /// Requested timeout; capped by the server's `job_timeout_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    Http { http: HttpSpec },
    Shell { shell: ShellSpec },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSpec {
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSpec {
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

fn default_interval() -> u64 {
    DEFAULT_INTERVAL_SECS
}

fn default_method() -> String {
    "GET".to_string()
}

impl Job {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Effective timeout: the job's request, never above `max_secs`.
    pub fn timeout(&self, max_secs: u64) -> Duration {
        Duration::from_secs(self.timeout_secs.map_or(max_secs, |t| t.clamp(1, max_secs)))
    }

    /// Reject jobs that cannot run or would misbehave.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty() && self.name.len() <= MAX_NAME_LEN,
            "job name must be 1-{MAX_NAME_LEN} characters"
        );
        ensure!(
            self.name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
            "job name may only contain letters, digits, '-', '_' and '.'"
        );
        ensure!(self.interval_secs > 0, "interval_secs must be > 0");
        match &self.action {
            Action::Http { http } => {
                ensure!(
                    http.url.starts_with("http://") || http.url.starts_with("https://"),
                    "http url must start with http:// or https://"
                );
                let method = http.method.to_ascii_uppercase();
                ensure!(
                    HTTP_METHODS.contains(&method.as_str()),
                    "unsupported http method {method}"
                );
            }
            Action::Shell { shell } => ensure!(!shell.cmd.is_empty(), "shell cmd is required"),
        }
        Ok(())
    }
}

/// Executes jobs with shared resources.
#[derive(Clone)]
pub struct Runner {
    http: reqwest::Client,
    max_timeout_secs: u64,
    max_output: usize,
}

impl Runner {
    pub fn new(max_timeout_secs: u64, max_output: usize) -> Result<Self> {
        let http = crate::http_client().build()?;
        Ok(Self {
            http,
            max_timeout_secs,
            max_output,
        })
    }

    /// Run `job` and return its (capped) report.
    pub async fn run(&self, job: &Job) -> Result<String> {
        let timeout = job.timeout(self.max_timeout_secs);
        let work = async {
            match &job.action {
                Action::Http { http } => self.run_http(&job.input, http).await,
                Action::Shell { shell } => self.run_shell(&job.input, shell).await,
            }
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| anyhow!("timed out after {}s", timeout.as_secs()))?
    }

    async fn run_http(&self, input: &BTreeMap<String, Value>, spec: &HttpSpec) -> Result<String> {
        let vars = vars(input);
        let url = render(&spec.url, &vars, true);
        let method = reqwest::Method::from_bytes(spec.method.to_ascii_uppercase().as_bytes())?;
        let mut req = self.http.request(method, &url);
        for (k, v) in &spec.headers {
            req = req.header(k, render(v, &vars, false));
        }
        if let Some(body) = &spec.body {
            req = req.body(render(body, &vars, false));
        }
        let mut resp = req.send().await.map_err(reqwest::Error::without_url)?;
        let status = resp.status();
        let mut body = Capped::new(self.max_output);
        while let Some(chunk) = resp.chunk().await? {
            if !body.push(&chunk) {
                break;
            }
        }
        Ok(format!("status: {status}\n{}", body.finish()))
    }

    async fn run_shell(&self, input: &BTreeMap<String, Value>, spec: &ShellSpec) -> Result<String> {
        let vars = vars(input);
        let mut child = tokio::process::Command::new(&spec.cmd)
            .args(spec.args.iter().map(|a| render(a, &vars, false)))
            .env("SUBSCRIBE_INPUT", serde_json::to_string(input)?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child dies with the future on timeout instead of leaking.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", spec.cmd))?;
        let stdout = child.stdout.take().context("no stdout pipe")?;
        let stderr = child.stderr.take().context("no stderr pipe")?;
        let (out, err, status) = tokio::try_join!(
            read_capped(stdout, self.max_output),
            read_capped(stderr, self.max_output),
            child.wait(),
        )?;
        let code = status
            .code()
            .map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
        let mut report = format!("exit: {code}\n{out}");
        if !err.is_empty() {
            report.push_str("\nstderr:\n");
            report.push_str(&err);
        }
        Ok(report)
    }
}

/// Read a stream to the end, keeping at most `max` bytes.
async fn read_capped(mut r: impl AsyncRead + Unpin, max: usize) -> std::io::Result<String> {
    let mut buf = Capped::new(max);
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Ok(buf.finish());
        }
        // Keep draining so the child never blocks on a full pipe.
        buf.push(&chunk[..n]);
    }
}

/// Byte buffer that stops growing at a limit and remembers truncation.
struct Capped {
    buf: Vec<u8>,
    max: usize,
    truncated: bool,
}

impl Capped {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
            truncated: false,
        }
    }

    /// Append; returns `false` once the limit is reached.
    fn push(&mut self, data: &[u8]) -> bool {
        let room = self.max.saturating_sub(self.buf.len());
        if data.len() > room {
            self.truncated = true;
        }
        self.buf.extend_from_slice(&data[..data.len().min(room)]);
        !self.truncated
    }

    fn finish(self) -> String {
        let mut s = String::from_utf8_lossy(&self.buf).into_owned();
        if self.truncated {
            // Lossy decoding may have split a multi-byte char into U+FFFD; fine.
            s.push_str("…[truncated]");
        }
        s
    }
}

fn vars(input: &BTreeMap<String, Value>) -> BTreeMap<&str, String> {
    input
        .iter()
        .map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.as_str(), s)
        })
        .collect()
}

/// Replace `{key}` placeholders; unknown keys are left untouched.
fn render(template: &str, vars: &BTreeMap<&str, String>, url_encode: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let found = after
            .find('}')
            .and_then(|close| vars.get(&after[..close]).map(|v| (close, v)));
        if let Some((close, value)) = found {
            if url_encode {
                out.push_str(&percent_encode(value));
            } else {
                out.push_str(value);
            }
            rest = &after[close + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// RFC 3986 percent-encoding of everything except unreserved characters.
fn percent_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Parse a job from user-supplied JSON, tolerating smart quotes that mobile
/// keyboards substitute for `"` and `'`.
pub fn parse(text: &str) -> Result<Job> {
    let normalized = text
        .replace(
            ['\u{201C}', '\u{201D}', '\u{201E}', '\u{00AB}', '\u{00BB}'],
            "\"",
        )
        .replace(['\u{2018}', '\u{2019}'], "'");
    let job: Job =
        serde_json::from_str(&normalized).map_err(|e| anyhow!("invalid job json: {e}"))?;
    if let Err(e) = job.validate() {
        bail!("invalid job: {e}");
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(v: Value) -> Job {
        serde_json::from_value(v).unwrap()
    }

    fn runner() -> Runner {
        Runner::new(10, 4000).unwrap()
    }

    #[tokio::test]
    async fn shell_job_renders_input_and_env() {
        let j = job(json!({
            "name": "t", "kind": "shell", "input": {"name": "world", "n": 3},
            "shell": {"cmd": "/bin/sh", "args": ["-c", "echo \"$0 {n} $SUBSCRIBE_INPUT\"", "{name}"]}
        }));
        let out = runner().run(&j).await.unwrap();
        assert!(out.starts_with("exit: 0\n"), "{out}");
        assert!(out.contains(r#"world 3 {"n":3,"name":"world"}"#), "{out}");
    }

    #[tokio::test]
    async fn shell_job_reports_exit_code_and_stderr() {
        let j = job(json!({
            "name": "t", "kind": "shell",
            "shell": {"cmd": "/bin/sh", "args": ["-c", "echo oops >&2; exit 3"]}
        }));
        let out = runner().run(&j).await.unwrap();
        assert!(
            out.contains("exit: 3") && out.contains("stderr:\noops"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn shell_job_times_out_and_is_capped() {
        let slow = job(json!({
            "name": "t", "kind": "shell", "timeout_secs": 1,
            "shell": {"cmd": "/bin/sleep", "args": ["5"]}
        }));
        let started = std::time::Instant::now();
        let err = runner().run(&slow).await.unwrap_err();
        assert!(err.to_string().contains("timed out after 1s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3));

        let chatty = job(json!({
            "name": "t", "kind": "shell",
            "shell": {"cmd": "/bin/sh", "args": ["-c", "yes | head -c 100000"]}
        }));
        let out = Runner::new(10, 100).unwrap().run(&chatty).await.unwrap();
        assert!(out.ends_with("…[truncated]") && out.len() < 200, "{out}");
    }

    #[test]
    fn render_substitutes_and_keeps_unknown() {
        let input = BTreeMap::from([("a".to_string(), json!("X y&z"))]);
        let v = vars(&input);
        assert_eq!(render("pre {a} {b} {a}", &v, false), "pre X y&z {b} X y&z");
        assert_eq!(render("?q={a}", &v, true), "?q=X%20y%26z");
        assert_eq!(render("unclosed {", &v, false), "unclosed {");
        assert_eq!(render("{{a}}", &v, false), "{X y&z}");
        assert_eq!(render("ünï {a}", &v, false), "ünï X y&z");
    }

    #[test]
    fn timeout_is_capped_by_server() {
        let mut j = job(json!({"name": "t", "kind": "shell", "shell": {"cmd": "/bin/true"}}));
        assert_eq!(j.timeout(15), Duration::from_secs(15));
        j.timeout_secs = Some(300);
        assert_eq!(j.timeout(15), Duration::from_secs(15));
        j.timeout_secs = Some(5);
        assert_eq!(j.timeout(15), Duration::from_secs(5));
    }

    #[test]
    fn parse_accepts_smart_quotes_and_validates() {
        let j = parse("{“name”:“demo”,“kind”:“http”,“input”:{“k”:“v”},“http”:{“url”:“https://example.com/?q={k}”}}").unwrap();
        assert_eq!(j.name(), "demo");
        assert_eq!(j.interval_secs, DEFAULT_INTERVAL_SECS);
        assert!(matches!(&j.action, Action::Http { http } if http.method == "GET"));

        for bad in [
            r#"{"name":"x","kind":"http","http":{"url":"ftp://x"}}"#,
            r#"{"name":"x","kind":"http","http":{"url":"https://x","method":"BREW"}}"#,
            r#"{"name":"","kind":"shell","shell":{"cmd":"/bin/true"}}"#,
            r#"{"name":"a b","kind":"shell","shell":{"cmd":"/bin/true"}}"#,
            r#"{"name":"x","kind":"shell","interval_secs":0,"shell":{"cmd":"/bin/true"}}"#,
            r#"{"name":"x","kind":"shell","shell":{"cmd":"/bin/true","typo":1}}"#,
            r#"{"name":"x","kind":"ftp"}"#,
        ] {
            assert!(parse(bad).is_err(), "accepted {bad}");
        }
    }
}
