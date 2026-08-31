//! shellaborate — collapse terminal->terminal loops (71.9% of 283k tool calls)
//! into one batched DAG call. Pure Rust, tokio, no network at run time.
//!
//! Contract: pipe a JSON `BatchRequest` to stdin, get structured JSON/NDJSON out.
//! git/gh commands are still shell under the hood; we only add structured
//! affordances (`git diff --stat` parsing, gh auth error surfacing).

pub mod update;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::mpsc;

pub const DEFAULT_CONCURRENCY: usize = 6;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_COMMANDS: usize = 50;
/// Per-stream (stdout/stderr) capture cap; past this output is truncated.
pub const MAX_STREAM_BYTES: usize = 5 * 1024 * 1024;

// ---------- protocol ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Stable id; defaults to index ("0", "1", ...).
    #[serde(default)]
    pub id: Option<String>,
    pub cmd: String,
    /// Working directory (relative resolves against invocation cwd; `..` rejected).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Merged over the sanitized parent environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// `sh` (default) or `bash`. The cmd string IS the program text — no extra
    /// escaping is applied on top (the caller crafts the shell syntax).
    #[serde(default)]
    pub shell: Option<String>,
}

/// One dependency edge: `to` runs only after `from` completes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    /// Linear shorthand: steps run in order, each chained to the previous.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// Explicit command list (pair with `dag` for dependencies; independent
    /// branches run concurrently).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Step>,
    /// Extra dependency edges between command ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dag: Vec<Edge>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Run independent branches even after a failure (default: stop scheduling).
    #[serde(default)]
    pub keep_going: bool,
    /// Operator override for denylisted commands.
    #[serde(default)]
    pub allow_dangerous: bool,
}

impl BatchRequest {
    /// Normalize steps+commands into one flat command list.
    pub fn flat_commands(&self) -> Vec<Step> {
        if !self.commands.is_empty() {
            self.commands.clone()
        } else {
            self.steps.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub id: String,
    pub cmd: String,
    /// "shell", "git", or "gh" — first-word classification.
    pub kind: String,
    pub exit: i64, // -1 = timed out / spawn failure
    /// Resolved working directory the step ran in (may come from `cwd` or a
    /// stripped `cd X &&` prefix).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Structured affordance for `git diff --stat` / `git show --stat` output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_stat: Option<Vec<GitStatLine>>,
}

/// One parsed row of `git diff --stat`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GitStatLine {
    pub path: String,
    pub changes: u64,
    pub bar: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchResponse {
    pub ok: bool,
    pub results: Vec<StepResult>,
    /// Steps never started because a dependency failed or the run cancelled.
    pub skipped: Vec<String>,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("empty request: provide `steps` or `commands`")]
    Empty,
    #[error("too many commands: {0} > cap {MAX_COMMANDS}")]
    TooManyCommands(usize),
    #[error("duplicate step id: {0:?}")]
    DuplicateId(String),
    #[error("unknown dependency: edge {from:?} -> {to:?}")]
    UnknownDep { from: String, to: String },
    #[error("dependency cycle involving: {0}")]
    Cycle(String),
    #[error("step {id:?}: shell must be \"sh\" or \"bash\", got {got:?}")]
    BadShell { id: String, got: String },
    #[error(
        "step {id:?}: denied by safety denylist: {reason} (pass --allow-dangerous to override)"
    )]
    Dangerous { id: String, reason: String },
}

// ---------- helpers ----------

/// Parse `git diff --stat`-style lines: `path | N +-`.
pub fn parse_git_stat(stdout: &str) -> Option<Vec<GitStatLine>> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some((path, rest)) = line.rsplit_once('|') else {
            continue;
        };
        if path.trim().is_empty()
            || line.contains("changed")
            || line.contains("insertion")
            || line.contains("deletion")
        {
            continue;
        }
        let rest = rest.trim_start();
        let Some(space) = rest.find(' ') else {
            continue;
        };
        let (num, bar) = rest.split_at(space);
        let Ok(changes) = num.trim().parse::<u64>() else {
            continue;
        };
        if changes == 0 {
            continue;
        }
        out.push(GitStatLine {
            path: path.trim().to_owned(),
            changes,
            bar: bar.trim().to_owned(),
        });
    }
    (!out.is_empty()).then_some(out)
}

/// Split a command into top-level shell segments (on && || ; | and newlines,
/// quote-aware) for denylist inspection.
fn segments(cmd: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let b = cmd.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
                i += 1;
            }
            '\n' | ';' => {
                segs.push(std::mem::take(&mut cur));
                i += 1;
            }
            '|' => {
                segs.push(std::mem::take(&mut cur));
                i += if i + 1 < b.len() && b[i + 1] == b'|' {
                    2
                } else {
                    1
                };
            }
            '&' => {
                if i + 1 < b.len() && b[i + 1] == b'&' {
                    segs.push(std::mem::take(&mut cur));
                    i += 2;
                } else {
                    i += 1; // background marker
                }
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    segs.push(cur);
    segs
}

/// Return Some(reason) if any segment matches the denylist.
pub fn dangerous_reason(cmd: &str) -> Option<String> {
    for seg in segments(cmd) {
        let t = seg.trim();
        if t.is_empty() {
            continue;
        }
        let toks: Vec<&str> = t.split_whitespace().collect();
        let rec = |a: &&str| {
            (a.starts_with('-')
                && !a.starts_with("--")
                && a[1..].chars().any(|c| c == 'r' || c == 'R'))
                || *a == "--recursive"
        };
        match toks[0] {
            "rm" if toks[1..].iter().any(rec)
                && toks[1..].iter().any(|a| *a == "/" || *a == "/*") =>
            {
                return Some("rm -rf /".into());
            }
            "shutdown" | "reboot" | "halt" | "poweroff" => return Some(toks[0].to_owned()),
            "init" if toks.len() > 1 && (toks[1] == "0" || toks[1] == "6") => {
                return Some(format!("init {}", toks[1]));
            }
            t0 if t0.starts_with("mkfs") => return Some(t0.to_owned()),
            "dd" if toks.iter().any(|a| a.starts_with("of=/dev/")) => {
                return Some("dd of=/dev/...".into());
            }
            _ => {}
        }
        if t.contains(":(){ :|:& };:") {
            return Some("forkbomb".into());
        }
    }
    None
}

/// Strip a simple `cd X && ` prefix, returning (cwd, remaining_cmd).
/// Only strips when the prefix is exactly `cd <path> &&` (quote-aware).
pub fn strip_cd_prefix(cmd: &str) -> (Option<String>, String) {
    let t = cmd.trim_start();
    if let Some(rest) = t.strip_prefix("cd ") {
        if let Some(pos) = find_top_level(rest, "&&") {
            let dir = rest[..pos].trim();
            if !dir.is_empty() && !dir.contains(';') && !dir.contains('|') && !dir.contains('`') {
                let dir = dir.trim_matches('"').trim_matches('\'').to_owned();
                return (Some(dir), rest[pos + 2..].trim_start().to_owned());
            }
        }
    }
    (None, cmd.to_owned())
}

/// First occurrence of `needle` at shell top level (not inside quotes).
fn find_top_level(hay: &str, needle: &str) -> Option<usize> {
    let b = hay.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        match quote {
            Some(q) if b[i] == q => quote = None,
            Some(_) => {}
            None if b[i] == b'"' || b[i] == b'\'' => quote = Some(b[i]),
            None if hay[i..].starts_with(needle) => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

fn classify(cmd: &str) -> String {
    let t = cmd.trim_start();
    let t = strip_cd_prefix(t).1;
    let first = t.split_whitespace().next().unwrap_or("");
    match first {
        "git" => "git".into(),
        "gh" => "gh".into(),
        _ => "shell".into(),
    }
}

/// Surface gh auth failures as a clean, actionable error string.
pub fn gh_auth_error(stderr: &str) -> Option<String> {
    if stderr.contains("gh: To get started with GitHub CLI") || stderr.contains("gh auth login") {
        Some("gh auth required: run `gh auth login` (token missing or expired)".into())
    } else if stderr.contains("HTTP 401") || stderr.contains("Bad credentials") {
        Some("gh auth invalid: token rejected (401) — refresh with `gh auth login`".into())
    } else {
        None
    }
}

/// Mask lines that look like secrets in captured output.
pub fn mask_secrets(s: &str) -> String {
    const KEY_NAMES: &[&str] = &[
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "HF_TOKEN",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "API_KEY",
        "SECRET",
        "PASSWORD",
        "AWS_SECRET_ACCESS_KEY",
        "PRIVATE KEY",
    ];
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let l = line.trim_end_matches('\n');
        if KEY_NAMES.iter().any(|k| l.contains(k))
            || l.contains("ghp_")
            || l.contains("github_pat_")
            || l.contains("xoxb-")
            || l.contains("Bearer ")
        {
            out.push_str("§§§§§§§§\n");
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn resolve_cwd(cwd: &Option<String>) -> Result<std::path::PathBuf> {
    match cwd {
        None => Ok(std::env::current_dir()?),
        Some(p) => {
            let path = std::path::Path::new(p);
            if path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                anyhow::bail!("cwd {p:?} contains '..' (path traversal rejected)");
            }
            Ok(if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            })
        }
    }
}

// ---------- execution ----------

struct Ctx {
    default_timeout: Duration,
}

/// Read a child stream with a bounded buffer. Keeps draining past the cap
/// (so the child never blocks on a full pipe) but stops storing.
async fn drain_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> (Vec<u8>, bool) {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n;
                if buf.len() < MAX_STREAM_BYTES {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
    (buf, total > MAX_STREAM_BYTES)
}

fn build_command(step: &Step, cwd: &std::path::Path) -> Result<Command> {
    let mut cmd = match step.shell.as_deref() {
        None | Some("sh") => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&step.cmd);
            c
        }
        Some("bash") => {
            let mut c = Command::new("bash");
            c.arg("-c").arg(&step.cmd);
            c
        }
        Some(other) => {
            return Err(anyhow::Error::new(BatchError::BadShell {
                id: step.id.clone().unwrap_or_default(),
                got: other.to_owned(),
            }));
        }
    };
    cmd.current_dir(cwd);
    // Sanitized environment: no ambient secret exfiltration, explicit allowlist.
    cmd.env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("SHELLABORATE", "1");
    for k in ["LANG", "LC_ALL", "TERM", "TMPDIR", "USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    for (k, v) in &step.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group so a timeout can kill the whole tree (sh + its
    // grandchildren); without this an orphaned `sleep` keeps the output pipes
    // open and the caller blocks past the deadline.
    #[cfg(unix)]
    cmd.process_group(0);
    Ok(cmd)
}

/// Kill the child's entire process group (SIGKILL). Falls back to nothing on
/// non-unix, where `start_kill` on the direct child is the best we can do.
#[cfg(unix)]
fn kill_process_group(child_id: u32) {
    unsafe {
        // pgid == child pid because of process_group(0).
        libc::kill(-(child_id as i32), libc::SIGKILL);
    }
}
#[cfg(not(unix))]
fn kill_process_group(_child_id: u32) {}

async fn run_step(step: &Step, ctx: &Ctx) -> StepResult {
    let id = step
        .id
        .clone()
        .unwrap_or_else(|| step.cmd.chars().take(24).collect());
    let started = Instant::now();
    let mk = |exit: i64, error: Option<String>, cwd: Option<String>| StepResult {
        kind: classify(&step.cmd),
        id: id.clone(),
        cmd: step.cmd.clone(),
        cwd,
        exit,
        stdout: None,
        stderr: None,
        stdout_truncated: false,
        stderr_truncated: false,
        elapsed_ms: started.elapsed().as_millis(),
        timeout: None,
        error,
        git_stat: None,
    };
    let timeout = Duration::from_millis(
        step.timeout_ms
            .unwrap_or(ctx.default_timeout.as_millis() as u64),
    );
    let (cd_cwd, _) = strip_cd_prefix(&step.cmd);
    let cwd = match resolve_cwd(&if step.cwd.is_some() {
        step.cwd.clone()
    } else {
        cd_cwd
    }) {
        Ok(p) => p,
        Err(e) => return mk(-1, Some(e.to_string()), step.cwd.clone()),
    };
    let mut command = match build_command(step, &cwd) {
        Ok(c) => c,
        Err(e) => return mk(-1, Some(e.to_string()), Some(cwd.display().to_string())),
    };
    let Ok(mut child) = command.spawn() else {
        return mk(
            -1,
            Some("spawn failed (shell not found?)".into()),
            Some(cwd.display().to_string()),
        );
    };
    let so = tokio::spawn(drain_capped(child.stdout.take().expect("piped stdout")));
    let se = tokio::spawn(drain_capped(child.stderr.take().expect("piped stderr")));
    let (exit, timed_out, error) = match tokio::time::timeout(timeout, child.wait()).await {
        Err(_) => {
            // Kill the whole process group first, then the child as fallback.
            let pid = child.id();
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            let _ = child.start_kill();
            (
                -1,
                true,
                Some(format!("timed out after {}ms", timeout.as_millis())),
            )
        }
        Ok(Err(e)) => (-1, false, Some(format!("wait failed: {e}"))),
        Ok(Ok(s)) => (s.code().map(i64::from).unwrap_or(-1), false, None),
    };
    // Grace period on the drains: normally pipes close as soon as the group
    // dies, but a wedged descriptor must never hang the batch.
    let grace = Duration::from_secs(5);
    let (so_bytes, so_trunc) = match tokio::time::timeout(grace, so).await {
        Ok(Ok(pair)) => pair,
        _ => (Vec::new(), false),
    };
    let (se_bytes, se_trunc) = match tokio::time::timeout(grace, se).await {
        Ok(Ok(pair)) => pair,
        _ => (Vec::new(), false),
    };
    let stdout_s = mask_secrets(&String::from_utf8_lossy(&so_bytes));
    let stderr_s = mask_secrets(&String::from_utf8_lossy(&se_bytes));
    let git_stat = if (step.cmd.contains("diff") || step.cmd.contains("show"))
        && step.cmd.contains("--stat")
    {
        parse_git_stat(&stdout_s)
    } else {
        None
    };
    let mut error = error;
    if step.cmd.trim_start().starts_with("gh ") {
        if let Some(auth) = gh_auth_error(&stderr_s) {
            error = Some(auth);
        }
    }
    StepResult {
        id,
        cmd: step.cmd.clone(),
        kind: classify(&step.cmd),
        cwd: Some(cwd.display().to_string()),
        exit,
        stdout: (!stdout_s.trim().is_empty()).then_some(stdout_s),
        stderr: (!stderr_s.trim().is_empty()).then_some(stderr_s),
        stdout_truncated: so_trunc,
        stderr_truncated: se_trunc,
        elapsed_ms: started.elapsed().as_millis(),
        timeout: timed_out.then_some(true),
        error,
        git_stat,
    }
}

// ---------- validation ----------

/// Validate the request; returns flat steps (with default ids) and all edges
/// (linear chains included).
pub fn validate(req: &BatchRequest) -> Result<(Vec<Step>, Vec<Edge>), BatchError> {
    let mut steps = req.flat_commands();
    if steps.is_empty() {
        return Err(BatchError::Empty);
    }
    if steps.len() > MAX_COMMANDS {
        return Err(BatchError::TooManyCommands(steps.len()));
    }
    for (i, s) in steps.iter_mut().enumerate() {
        if s.id.is_none() {
            s.id = Some(i.to_string());
        }
    }
    let mut edges: Vec<Edge> = Vec::new();
    // Linear shorthand: chain steps to the previous one.
    if !req.steps.is_empty() && req.commands.is_empty() {
        for w in 1..steps.len() {
            edges.push(Edge {
                from: steps[w - 1]
                    .id
                    .clone()
                    .unwrap_or_else(|| (w - 1).to_string()),
                to: steps[w].id.clone().unwrap_or_else(|| w.to_string()),
            });
        }
    }
    for s in &steps {
        if let Some(sh) = &s.shell {
            if sh != "sh" && sh != "bash" {
                return Err(BatchError::BadShell {
                    id: s.id.clone().unwrap_or_default(),
                    got: sh.clone(),
                });
            }
        }
        if !req.allow_dangerous {
            if let Some(reason) = dangerous_reason(&s.cmd) {
                return Err(BatchError::Dangerous {
                    id: s.id.clone().unwrap_or_default(),
                    reason,
                });
            }
        }
    }
    let mut seen = HashSet::new();
    for s in &steps {
        let id = s.id.clone().unwrap_or_default();
        if !seen.insert(id.clone()) {
            return Err(BatchError::DuplicateId(id));
        }
    }
    for e in &req.dag {
        if !seen.contains(&e.from) || !seen.contains(&e.to) {
            return Err(BatchError::UnknownDep {
                from: e.from.clone(),
                to: e.to.clone(),
            });
        }
        if e.from == e.to {
            return Err(BatchError::Cycle(e.to.clone()));
        }
        edges.push(e.clone());
    }
    // Cycle check (Kahn) over deduplicated edges.
    let mut indeg: HashMap<&str, usize> = steps
        .iter()
        .map(|s| (s.id.as_deref().unwrap(), 0))
        .collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut dedup: HashSet<(&str, &str)> = HashSet::new();
    for e in &edges {
        if dedup.insert((e.from.as_str(), e.to.as_str())) {
            *indeg.get_mut(e.to.as_str()).unwrap() += 1;
            adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
        }
    }
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut visited = 0;
    let mut qi = 0;
    while qi < queue.len() {
        let node = queue[qi];
        qi += 1;
        visited += 1;
        if let Some(nbrs) = adj.get(node) {
            for &m in nbrs {
                let d = indeg.get_mut(m).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push(m);
                }
            }
        }
    }
    if visited != steps.len() {
        let stuck: Vec<String> = indeg
            .iter()
            .filter(|(_, d)| **d > 0)
            .map(|(k, _)| (*k).to_owned())
            .collect();
        return Err(BatchError::Cycle(stuck.join(", ")));
    }
    Ok((steps, edges))
}

/// Compute the execution plan (for dry-run): id, cmd, cwd, immediate deps.
pub fn plan(req: &BatchRequest) -> Result<Vec<serde_json::Value>, BatchError> {
    let (steps, edges) = validate(req)?;
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &edges {
        deps.entry(e.to.as_str()).or_default().push(e.from.as_str());
    }
    Ok(steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "cmd": s.cmd,
                "cwd": s.cwd,
                "after": deps.get(s.id.as_deref().unwrap()).cloned().unwrap_or_default(),
            })
        })
        .collect())
}

// ---------- scheduler ----------

/// Execute the DAG: independent branches run concurrently (bounded by
/// `concurrency`), dependents wait for all parents, scheduling stops on the
/// first failure unless `keep_going`. If `stream_tx` is set, each completed
/// step is sent on it as it finishes (for ndjson streaming).
pub async fn run_batch_streamed(
    req: &BatchRequest,
    stream_tx: Option<mpsc::UnboundedSender<StepResult>>,
) -> Result<BatchResponse> {
    let started = Instant::now();
    let (steps, edges) = validate(req).map_err(anyhow::Error::from)?;
    let ctx = Arc::new(Ctx {
        default_timeout: Duration::from_millis(req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
    });
    let n = steps.len();
    let idx: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_deref().unwrap(), i))
        .collect();
    let mut deps = vec![0usize; n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut dedup: HashSet<(usize, usize)> = HashSet::new();
    for e in &edges {
        let (f, t) = (idx[e.from.as_str()], idx[e.to.as_str()]);
        if dedup.insert((f, t)) {
            deps[t] += 1;
            children[f].push(t);
        }
    }
    let results: Arc<tokio::sync::Mutex<Vec<Option<StepResult>>>> =
        Arc::new(tokio::sync::Mutex::new((0..n).map(|_| None).collect()));
    let sem = Arc::new(tokio::sync::Semaphore::new(
        req.concurrency
            .unwrap_or(DEFAULT_CONCURRENCY)
            .clamp(1, MAX_COMMANDS),
    ));
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<usize>();
    let cancelled = Arc::new(AtomicBool::new(false));
    let completed = AtomicUsize::new(0);

    let mut scheduled = vec![false; n];
    let mut running = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut failed = false;

    while completed.load(Ordering::SeqCst) < n {
        if failed && !req.keep_going {
            cancelled.store(true, Ordering::SeqCst);
            // Never-scheduled nodes are skipped; in-flight ones drain below.
            for i in 0..n {
                if !scheduled[i] {
                    skipped.push(steps[i].id.clone().unwrap_or_else(|| i.to_string()));
                    scheduled[i] = true;
                    completed.fetch_add(1, Ordering::SeqCst);
                }
            }
            while running > 0 {
                let i = done_rx.recv().await.expect("in-flight task");
                running -= 1;
                completed.fetch_add(1, Ordering::SeqCst);
                let _ = i;
            }
            break;
        }
        // Launch every ready, unscheduled node.
        for i in 0..n {
            if !scheduled[i] && deps[i] == 0 {
                scheduled[i] = true;
                running += 1;
                let step = steps[i].clone();
                let ctx = ctx.clone();
                let results = results.clone();
                let sem = sem.clone();
                let done_tx = done_tx.clone();
                let cancelled = cancelled.clone();
                tokio::spawn(async move {
                    if cancelled.load(Ordering::SeqCst) {
                        let _ = done_tx.send(i); // never ran; unblock the scheduler
                        return;
                    }
                    let _permit = sem.acquire_owned().await.expect("semaphore open");
                    if cancelled.load(Ordering::SeqCst) {
                        // Cancelled while queued on the semaphore.
                        let _ = done_tx.send(i);
                        return;
                    }
                    let r = run_step(&step, &ctx).await;
                    results.lock().await[i] = Some(r);
                    let _ = done_tx.send(i);
                });
            }
        }
        if running == 0 {
            // Nothing launchable left (post cycle-check this means all done/skipped).
            for i in 0..n {
                if !scheduled[i] {
                    skipped.push(steps[i].id.clone().unwrap_or_else(|| i.to_string()));
                    scheduled[i] = true;
                    completed.fetch_add(1, Ordering::SeqCst);
                }
            }
            break;
        }
        let i = done_rx.recv().await.expect("running task sends");
        running -= 1;
        completed.fetch_add(1, Ordering::SeqCst);
        let r = results.lock().await[i]
            .clone()
            .expect("result stored before send");
        if r.exit != 0 {
            failed = true;
        }
        if let Some(tx) = &stream_tx {
            let _ = tx.send(r);
        }
        for &c in &children[i] {
            deps[c] -= 1;
        }
    }
    drop(done_tx);

    let mut final_results: Vec<StepResult> =
        results.lock().await.iter().flatten().cloned().collect();
    // Declaration order for stable output.
    final_results.sort_by_key(|r| {
        steps
            .iter()
            .position(|s| s.id.as_deref() == Some(r.id.as_str()))
            .unwrap_or(usize::MAX)
    });
    let ok = !final_results.is_empty()
        && final_results.iter().all(|r| r.exit == 0)
        && skipped.is_empty();
    Ok(BatchResponse {
        ok,
        results: final_results,
        skipped,
        elapsed_ms: started.elapsed().as_millis(),
        error: None,
    })
}

/// Convenience wrapper with no streaming.
pub async fn run_batch(req: &BatchRequest) -> Result<BatchResponse> {
    run_batch_streamed(req, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(cmd: &str) -> Step {
        Step {
            id: None,
            cmd: cmd.to_owned(),
            cwd: None,
            env: HashMap::new(),
            timeout_ms: None,
            shell: None,
        }
    }
    fn sid(id: &str, cmd: &str) -> Step {
        Step {
            id: Some(id.to_owned()),
            cmd: cmd.to_owned(),
            cwd: None,
            env: HashMap::new(),
            timeout_ms: None,
            shell: None,
        }
    }
    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn strip_cd() {
        let (cwd, rest) = strip_cd_prefix("cd /tmp && echo hi");
        assert_eq!(cwd.as_deref(), Some("/tmp"));
        assert_eq!(rest, "echo hi");
        let (cwd, rest) = strip_cd_prefix("echo a && cd /tmp && echo b");
        assert_eq!(cwd, None);
        assert_eq!(rest, "echo a && cd /tmp && echo b");
        let (cwd, rest) = strip_cd_prefix("cd 'my dir' && ls");
        assert_eq!(cwd.as_deref(), Some("my dir"));
        assert_eq!(rest, "ls");
    }

    #[test]
    fn git_stat_parse() {
        let s = " src/lib.rs   | 12 ++++++-----\n README.md    |  2 +-\n 2 files changed, 8 insertions(+), 6 deletions(-)";
        let g = parse_git_stat(s).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].path, "src/lib.rs");
        assert_eq!(g[0].changes, 12);
        assert_eq!(g[1].path, "README.md");
        assert!(parse_git_stat("no stat here").is_none());
    }

    #[test]
    fn denylist_hits_and_misses() {
        assert!(dangerous_reason("rm -rf /").is_some());
        assert!(dangerous_reason("cd / && rm -rf /").is_some());
        assert!(dangerous_reason("echo hi && shutdown -h now").is_some());
        assert!(dangerous_reason("shutdown").is_some());
        assert!(dangerous_reason("mkfs.ext4 /dev/sda1").is_some());
        assert!(dangerous_reason("dd if=/dev/zero of=/dev/sda").is_some());
        assert!(dangerous_reason("echo shutdown").is_none());
        assert!(dangerous_reason("rm -rf ./build").is_none());
        assert!(dangerous_reason("echo hi").is_none());
    }

    #[test]
    fn mask() {
        let m = mask_secrets("GH_TOKEN=ghp_abcdef\nhello");
        assert!(m.contains("§§§§§§§§"));
        assert!(m.contains("hello"));
        assert!(!m.contains("ghp_abcdef"));
        assert_eq!(mask_secrets("plain"), "plain\n");
    }

    #[test]
    fn gh_auth_surfacing() {
        assert!(
            gh_auth_error("gh: To get started with GitHub CLI, please run: gh auth login")
                .is_some()
        );
        assert!(gh_auth_error("HTTP 401: Bad credentials").is_some());
        assert!(gh_auth_error("normal stderr").is_none());
    }

    #[tokio::test]
    async fn linear_chain_runs_in_order() {
        let req = BatchRequest {
            steps: vec![step("echo a"), step("echo b")],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.results.len(), 2);
        assert_eq!(resp.results[0].stdout.as_deref(), Some("a\n"));
        assert_eq!(resp.results[1].stdout.as_deref(), Some("b\n"));
        assert!(resp.skipped.is_empty());
    }

    #[tokio::test]
    async fn diamond_dag() {
        let req = BatchRequest {
            commands: vec![
                sid("a", "echo a"),
                sid("b", "sleep 0.1 && echo b"),
                sid("c", "echo c"),
                sid("d", "echo d"),
            ],
            dag: vec![
                edge("a", "b"),
                edge("a", "c"),
                edge("b", "d"),
                edge("c", "d"),
            ],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.results.len(), 4);
        assert_eq!(resp.results[3].id, "d");
    }

    #[tokio::test]
    async fn independent_branches_run_concurrently() {
        // Two 300ms sleeps on parallel branches; total wall clock must be well under 600ms.
        let req = BatchRequest {
            commands: vec![
                sid("a", "sleep 0.3 && echo a"),
                sid("b", "sleep 0.3 && echo b"),
            ],
            concurrency: Some(2),
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(resp.ok);
        assert!(
            resp.elapsed_ms < 600,
            "took {}ms — not concurrent",
            resp.elapsed_ms
        );
    }

    #[tokio::test]
    async fn failure_skips_dependents() {
        let req = BatchRequest {
            commands: vec![sid("a", "exit 3"), sid("b", "echo never")],
            dag: vec![edge("a", "b")],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.results[0].exit, 3);
        let ran: Vec<&str> = resp.results.iter().map(|r| r.id.as_str()).collect();
        let skipped: Vec<&str> = resp.skipped.iter().map(|s| s.as_str()).collect();
        assert!(
            skipped.contains(&"b"),
            "dependent must be skipped: {skipped:?}"
        );
        assert!(!ran.contains(&"b"));
    }

    #[tokio::test]
    async fn failure_stops_unscheduled_work() {
        // a fails fast; c sleeps, so its successor d is scheduled only after
        // the failure is known and must be skipped (keep_going=false).
        let req = BatchRequest {
            commands: vec![
                sid("a", "exit 3"),
                sid("c", "sleep 0.3 && echo c"),
                sid("d", "echo d"),
            ],
            dag: vec![edge("c", "d")],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(!resp.ok);
        let skipped: Vec<&str> = resp.skipped.iter().map(|s| s.as_str()).collect();
        assert!(
            skipped.contains(&"d"),
            "unscheduled successor must be skipped: {skipped:?}"
        );
    }

    #[tokio::test]
    async fn keep_going_runs_independent_branches() {
        let req = BatchRequest {
            commands: vec![sid("a", "exit 3"), sid("b", "echo survivor")],
            keep_going: true,
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(!resp.ok);
        let b = resp
            .results
            .iter()
            .find(|r| r.id == "b")
            .expect("b must run with keep_going");
        assert_eq!(b.stdout.as_deref(), Some("survivor\n"));
    }

    #[tokio::test]
    async fn timeout_kills() {
        let mut s = step("sleep 5 && echo late");
        s.timeout_ms = Some(200);
        let req = BatchRequest {
            steps: vec![s],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(!resp.ok);
        let r = &resp.results[0];
        assert_eq!(r.exit, -1);
        assert_eq!(r.timeout, Some(true));
        assert!(r.elapsed_ms < 2000, "kill took too long: {}", r.elapsed_ms);
    }

    #[tokio::test]
    async fn env_merge_and_cd_strip() {
        let mut s = step("echo $FOO in $(pwd)");
        s.env.insert("FOO".into(), "bar".into());
        s.cmd = format!("cd /tmp && {}", s.cmd);
        let req = BatchRequest {
            steps: vec![s],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.results[0].stdout.as_deref(), Some("bar in /tmp\n"));
    }

    #[test]
    fn rejects_over_cap() {
        let steps: Vec<Step> = (0..=MAX_COMMANDS)
            .map(|i| step(&format!("echo {i}")))
            .collect();
        let req = BatchRequest {
            steps,
            ..Default::default()
        };
        assert!(matches!(
            validate(&req),
            Err(BatchError::TooManyCommands(_))
        ));
    }

    #[test]
    fn rejects_cycle() {
        let req = BatchRequest {
            commands: vec![sid("a", "echo x"), sid("b", "echo y")],
            dag: vec![edge("a", "b"), edge("b", "a")],
            ..Default::default()
        };
        assert!(matches!(validate(&req), Err(BatchError::Cycle(_))));
    }

    #[test]
    fn rejects_unknown_dep_and_dup_and_bad_shell() {
        let req = BatchRequest {
            steps: vec![step("echo x")],
            dag: vec![edge("0", "nope")],
            ..Default::default()
        };
        assert!(matches!(validate(&req), Err(BatchError::UnknownDep { .. })));
        let req = BatchRequest {
            commands: vec![sid("a", "echo x"), sid("a", "echo y")],
            ..Default::default()
        };
        assert!(matches!(validate(&req), Err(BatchError::DuplicateId(_))));
        let mut s = step("echo x");
        s.shell = Some("zsh".into());
        let req = BatchRequest {
            steps: vec![s],
            ..Default::default()
        };
        assert!(matches!(validate(&req), Err(BatchError::BadShell { .. })));
        assert!(matches!(
            validate(&BatchRequest::default()),
            Err(BatchError::Empty)
        ));
    }

    #[test]
    fn dangerous_needs_override() {
        let req = BatchRequest {
            steps: vec![step("rm -rf /")],
            ..Default::default()
        };
        assert!(matches!(validate(&req), Err(BatchError::Dangerous { .. })));
        let req2 = BatchRequest {
            steps: vec![step("rm -rf /")],
            allow_dangerous: true,
            ..Default::default()
        };
        assert!(validate(&req2).is_ok());
    }

    #[tokio::test]
    async fn cwd_traversal_rejected() {
        let mut s = step("echo x");
        s.cwd = Some("../../etc".into());
        let req = BatchRequest {
            steps: vec![s],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert!(!resp.ok);
        assert!(
            resp.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("traversal")
        );
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        let s = step("head -c 10485760 /dev/zero | tr '\\0' 'a'");
        let req = BatchRequest {
            steps: vec![s],
            timeout_ms: Some(20_000),
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        let r = &resp.results[0];
        assert!(r.stdout_truncated, "expected truncation flag");
        // Stored bytes stop at the cap; the last chunk may overshoot by up to
        // one 8KiB read, so allow a chunk of slack.
        assert!(
            r.stdout.as_ref().unwrap().len() <= MAX_STREAM_BYTES + 8192,
            "captured {} bytes",
            r.stdout.as_ref().unwrap().len()
        );
    }

    #[tokio::test]
    async fn streaming_yields_each_step() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = BatchRequest {
            commands: vec![sid("a", "echo a"), sid("b", "echo b")],
            ..Default::default()
        };
        let resp = run_batch_streamed(&req, Some(tx)).await.unwrap();
        assert!(resp.ok);
        let mut seen = Vec::new();
        while let Ok(r) = rx.try_recv() {
            seen.push(r.id);
        }
        assert_eq!(seen.len(), 2);
    }

    #[tokio::test]
    async fn env_sanitized_no_ambient_secrets() {
        // SHELLABORATE sanitize: an ambient var that is NOT in the allowlist must not leak.
        let req = BatchRequest {
            steps: vec![step("echo \"[$AMBIENT_SECRET]\"")],
            ..Default::default()
        };
        let resp = run_batch(&req).await.unwrap();
        assert_eq!(resp.results[0].stdout.as_deref(), Some("[]\n"));
    }
}
