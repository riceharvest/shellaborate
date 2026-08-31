//! shellaborate CLI: pipe a JSON BatchRequest to stdin, get structured output.
//! Batch shell for AI agents — collapses terminal->terminal loops into one call.

use clap::{Command as ClapCommand, CommandFactory, Parser, ValueEnum};
use clap_complete::{Shell, generate};
use std::io::Read;
use std::process::ExitCode;

#[derive(ValueEnum, Clone, Copy, Debug)]
enum OutputFormat {
    /// Single JSON object with all results.
    Json,
    /// Newline-delimited JSON: one object per completed step as it finishes,
    /// then a final summary object. For long runs.
    Ndjson,
}

#[derive(Parser, Debug)]
#[command(
    name = "shellaborate",
    version,
    about = "Batch shell DAG for AI agents: run many shell/git/gh commands in one call",
    after_help = "Pipe a JSON BatchRequest to stdin:\n  echo '{\"steps\":[{\"cmd\":\"echo hi\"}]}' | shellaborate\n\nGenerate agent tool schema:\n  shellaborate --emit hermes-tool.json"
)]
struct Cli {
    /// Read BatchRequest JSON from this file instead of stdin ("-" = stdin).
    #[arg(long, value_name = "FILE", default_value = "-")]
    input: String,

    /// Print the execution plan (id, cmd, deps) without running anything.
    #[arg(long)]
    dry_run: bool,

    /// Output format: json (single object) or ndjson (streamed per step).
    #[arg(long, value_enum, default_value = "json")]
    output: OutputFormat,

    /// Pretty-print JSON output (not with ndjson).
    #[arg(long)]
    pretty: bool,

    /// Permit denylisted commands (rm -rf /, shutdown, mkfs, dd of=/dev/...).
    #[arg(long)]
    allow_dangerous: bool,

    /// Emit a generated artifact and exit: hermes-tool.json, man, or a shell
    /// completion (bash/zsh/fish).
    #[arg(long, value_name = "TARGET")]
    emit: Option<String>,

    /// Self-update from GitHub Releases (checksum-verified).
    #[arg(long)]
    update: bool,
}

fn hermes_tool_json() -> serde_json::Value {
    serde_json::json!({
        "name": "shellaborate",
        "description": "Batch shell DAG executor. Runs multiple shell/git/gh commands in one call with dependency ordering, bounded concurrency, timeouts, and structured JSON results. Prefer one shellaborate call over N terminal calls.",
        "version": env!("CARGO_PKG_VERSION"),
        "input": {
            "type": "object",
            "required": [],
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "Linear shorthand: runs in order, each step depends on the previous",
                    "items": { "$ref": "#/step" }
                },
                "commands": {
                    "type": "array",
                    "description": "Commands for a DAG; pair with `dag` edges. Independent commands run concurrently.",
                    "items": { "$ref": "#/step" }
                },
                "dag": {
                    "type": "array",
                    "description": "Dependency edges between command ids: `to` runs after `from` completes",
                    "items": {
                        "type": "object",
                        "required": ["from", "to"],
                        "properties": { "from": {"type": "string"}, "to": {"type": "string"} }
                    }
                },
                "concurrency": { "type": "integer", "description": "Max parallel branches (default 6, cap 50)" },
                "timeout_ms": { "type": "integer", "description": "Default per-step timeout in ms (default 30000)" },
                "keep_going": { "type": "boolean", "description": "Run independent branches after a failure (default false)" },
                "allow_dangerous": { "type": "boolean", "description": "Permit denylisted destructive commands" }
            }
        },
        "step": {
            "type": "object",
            "required": ["cmd"],
            "properties": {
                "id": { "type": "string", "description": "Stable id for dag edges (defaults to array index)" },
                "cmd": { "type": "string", "description": "Shell command text (sh -c; no extra escaping applied)" },
                "cwd": { "type": "string", "description": "Working directory (no '..' allowed)" },
                "env": { "type": "object", "description": "Extra environment variables" },
                "timeout_ms": { "type": "integer" },
                "shell": { "type": "string", "enum": ["sh", "bash"] }
            }
        },
        "output": {
            "type": "object",
            "properties": {
                "ok": { "type": "boolean", "description": "true iff every step exited 0 and none were skipped" },
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "cmd": {"type": "string"},
                            "kind": {"type": "string", "description": "shell | git | gh"},
                            "exit": {"type": "integer", "description": "-1 on timeout/spawn failure"},
                            "stdout": {"type": "string"},
                            "stderr": {"type": "string"},
                            "stdout_truncated": {"type": "boolean"},
                            "stderr_truncated": {"type": "boolean"},
                            "elapsed_ms": {"type": "integer"},
                            "timeout": {"type": "boolean"},
                            "error": {"type": "string"},
                            "git_stat": {"type": "array", "description": "Parsed rows for `git diff --stat` / `git show --stat`"}
                        }
                    }
                },
                "skipped": { "type": "array", "items": {"type": "string"}, "description": "Step ids never started (dependency failed or run cancelled)" },
                "elapsed_ms": { "type": "integer" }
            }
        },
        "notes": [
            "stdout/stderr are capped at 5 MiB per stream and flagged *_truncated.",
            "Environment is sanitized: only PATH/HOME/TEMP-ish vars pass through; inject via step.env.",
            "Lines that look like secrets in output are masked.",
            "Dangerous commands are denied unless --allow-dangerous."
        ]
    })
}

fn write_json(v: &serde_json::Value, pretty: bool) {
    if pretty {
        println!("{}", serde_json::to_string_pretty(v).expect("serialize"));
    } else {
        println!("{}", serde_json::to_string(v).expect("serialize"));
    }
}

async fn run_batch_io(cli: &Cli) -> anyhow::Result<ExitCode> {
    let mut input = String::new();
    if cli.input == "-" {
        std::io::stdin().read_to_string(&mut input)?;
    } else {
        input = std::fs::read_to_string(&cli.input)?;
    }
    let mut req: shellaborate::BatchRequest = serde_json::from_str(&input)
        .map_err(|e| anyhow::anyhow!("invalid BatchRequest JSON: {e}"))?;
    // CLI flag wins over the JSON field.
    req.allow_dangerous = req.allow_dangerous || cli.allow_dangerous;

    if cli.dry_run {
        let plan = shellaborate::plan(&req).map_err(|e| anyhow::anyhow!("{e}"))?;
        write_json(
            &serde_json::json!({ "ok": true, "dry_run": true, "plan": plan }),
            cli.pretty,
        );
        return Ok(ExitCode::SUCCESS);
    }

    match cli.output {
        OutputFormat::Json => {
            let resp = shellaborate::run_batch(&req).await?;
            let ok = resp.ok;
            write_json(&serde_json::to_value(&resp)?, cli.pretty);
            Ok(if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        OutputFormat::Ndjson => {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let req = req.clone();
            let handle =
                tokio::spawn(async move { shellaborate::run_batch_streamed(&req, Some(tx)).await });
            while let Some(step) = rx.recv().await {
                println!("{}", serde_json::to_string(&step)?);
            }
            let resp = handle.await??;
            let ok = resp.ok;
            // Final summary object.
            println!("{}", serde_json::to_string(&serde_json::to_value(&resp)?)?);
            Ok(if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

fn emit(target: &str, cmd: &ClapCommand) -> anyhow::Result<ExitCode> {
    match target {
        "hermes-tool.json" => {
            write_json(&hermes_tool_json(), true);
            Ok(ExitCode::SUCCESS)
        }
        "man" => {
            let man = clap_mangen::Man::new(cmd.clone());
            man.render(&mut std::io::stdout())?;
            Ok(ExitCode::SUCCESS)
        }
        "bash" | "zsh" | "fish" => {
            let shell = match Shell::from_str(target, true) {
                Ok(s) => s,
                Err(_) => anyhow::bail!("unknown shell {target:?}"),
            };
            let mut c = cmd.clone();
            generate(shell, &mut c, "shellaborate", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
        other => anyhow::bail!(
            "unknown --emit target {other:?} (supported: hermes-tool.json, man, bash, zsh, fish)"
        ),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cmd = Cli::command();

    if cli.update {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        return match rt.block_on(shellaborate::update::run_update()) {
            Ok(m) => {
                println!("{m}");
                ExitCode::SUCCESS
            }
            Err(shellaborate::update::UpdateError::UpToDate(m)) => {
                println!("{m}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("shellaborate update: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if let Some(target) = &cli.emit {
        return match emit(target, &cmd) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("shellaborate: {e}");
                ExitCode::from(2)
            }
        };
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(run_batch_io(&cli)) {
        Ok(c) => c,
        Err(e) => {
            // Structured error on stdout, human summary on stderr.
            eprintln!("shellaborate: {e}");
            write_json(
                &serde_json::json!({ "ok": false, "results": [], "skipped": [], "elapsed_ms": 0, "error": e.to_string() }),
                false,
            );
            ExitCode::from(2)
        }
    }
}
