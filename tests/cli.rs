//! Integration tests: drive the real binary end-to-end (no network).
//! Each test builds the debug binary once via CARGO_BIN_EXE.

use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_shellaborate")
}

fn run_cli(args: &[&str], stdin: &str) -> (i32, String, String) {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shellaborate");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn help_and_version() {
    let (code, out, _) = run_cli(&["--help"], "");
    assert_eq!(code, 0);
    assert!(out.contains("Usage"), "help output: {out}");
    let (code, out, _) = run_cli(&["--version"], "");
    assert_eq!(code, 0);
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
}

#[test]
fn fixture_dag_runs_and_captures() {
    // echo a; echo b; (sleep 0.1 && echo c) after a; assert all captured.
    let req = r#"{
        "commands": [
            {"id": "a", "cmd": "echo a"},
            {"id": "b", "cmd": "echo b"},
            {"id": "c", "cmd": "sleep 0.1 && echo c"},
            {"id": "d", "cmd": "cat /nonexistent-file-xyz 2>/dev/null; echo d"}
        ],
        "dag": [{"from": "a", "to": "c"}]
    }"#;
    let (code, out, err) = run_cli(&[], req);
    assert_eq!(code, 0, "stderr: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON out");
    assert_eq!(v["ok"], serde_json::json!(true));
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["stdout"], serde_json::json!("a\n"));
    assert_eq!(results[1]["stdout"], serde_json::json!("b\n"));
    assert_eq!(results[2]["stdout"], serde_json::json!("c\n"));
    assert_eq!(results[0]["kind"], serde_json::json!("shell"));
    assert!(results[2]["elapsed_ms"].as_u64().unwrap() >= 90, "c slept");
}

#[test]
fn git_and_gh_kinds_classified() {
    let req = r#"{
        "steps": [
            {"id": "g", "cmd": "git --version"},
            {"id": "h", "cmd": "gh --version"}
        ]
    }"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let kinds: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["git", "gh"]);
}

#[test]
fn git_diff_stat_parsing() {
    // Build a real git repo in a temp dir and parse actual diff --stat output.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git")
    };
    assert!(git(&["init", "-q"]).status.success());
    std::fs::write(repo.join("f.txt"), "one\ntwo\nthree\n").unwrap();
    assert!(git(&["add", "."]).status.success());
    assert!(git(&["commit", "-qm", "init"]).status.success());
    std::fs::write(repo.join("f.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(repo.join("g.txt"), "new\n").unwrap();
    assert!(git(&["add", "."]).status.success());

    let req = r#"{"steps":[{"cmd":"git diff --cached --stat"}]}"#;
    let mut child = Command::new(bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(repo)
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(req.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let stat = &v["results"][0]["git_stat"];
    assert!(stat.is_array(), "git_stat parsed: {v}");
    let paths: Vec<&str> = stat
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["f.txt", "g.txt"]);
}

#[test]
fn gh_failure_surfaces_cleanly() {
    // gh pointed at a dead loopback host via step.env (the sanitized child env
    // only inherits an allowlist, so test overrides ride in step.env). Must
    // exit non-zero, emit valid JSON with ok=false, never panic. Loopback only
    // — no external network in tests.
    let req = r#"{"steps":[{"cmd":"gh api user","env":{"GH_CONFIG_DIR":"/tmp","GH_HOST":"localhost:1"}}]}"#;
    let mut child = Command::new(bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(req.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("valid JSON on failure: {stdout}");
    assert_eq!(v["ok"], serde_json::json!(false), "{v}");
    assert_eq!(out.status.code(), Some(1));
    assert!(!stdout.contains("panicked"));
}

#[test]
fn dry_run_prints_plan_without_executing() {
    let req = r#"{
        "commands": [
            {"id": "a", "cmd": "touch /tmp/shellaborate-dryrun-marker"},
            {"id": "b", "cmd": "echo b"}
        ],
        "dag": [{"from": "a", "to": "b"}]
    }"#;
    let (code, out, _) = run_cli(&["--dry-run"], req);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["dry_run"], serde_json::json!(true));
    let plan = v["plan"].as_array().unwrap();
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[1]["after"], serde_json::json!(["a"]));
    assert!(
        !std::path::Path::new("/tmp/shellaborate-dryrun-marker").exists(),
        "dry-run must not execute"
    );
}

#[test]
fn denylist_rejects_and_override_allows() {
    let req = r#"{"steps":[{"cmd":"rm -rf /"}]}"#;
    let (code, out, err) = run_cli(&[], req);
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("denied by safety denylist"), "{err}");
    assert!(
        out.contains("denied by safety denylist"),
        "structured error on stdout: {out}"
    );

    let (code, _, _) = run_cli(&["--allow-dangerous"], req);
    assert_ne!(code, 2, "override accepted the request");
}

#[test]
fn bad_json_gives_structured_error() {
    let (code, out, err) = run_cli(&[], "{not json");
    assert_eq!(code, 2);
    assert!(err.contains("invalid BatchRequest JSON"), "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], serde_json::json!(false));
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("invalid BatchRequest")
    );
}

#[test]
fn ndjson_stream_mode() {
    let req = r#"{
        "commands": [
            {"id": "a", "cmd": "sleep 0.1 && echo a"},
            {"id": "b", "cmd": "echo b"}
        ]
    }"#;
    let (code, out, _) = run_cli(&["--output", "ndjson"], req);
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    // one line per finished step + one summary
    assert_eq!(lines.len(), 3, "{out}");
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first["id"].is_string() && first["exit"].is_i64());
    let summary: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(summary["ok"], serde_json::json!(true));
    assert!(summary["results"].is_array(), "final line is the summary");
}

#[test]
fn emit_hermes_tool_and_man_and_completions() {
    let (code, out, _) = run_cli(&["--emit", "hermes-tool.json"], "");
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid tool json");
    assert_eq!(v["name"], serde_json::json!("shellaborate"));
    assert!(v["input"]["properties"]["commands"].is_object());

    let (code, out, _) = run_cli(&["--emit", "man"], "");
    assert_eq!(code, 0);
    assert!(out.contains(".TH"), "man page: {out:.200}");

    for shell in ["bash", "zsh", "fish"] {
        let (code, out, _) = run_cli(&["--emit", shell], "");
        assert_eq!(code, 0, "{shell}");
        assert!(!out.is_empty(), "{shell} completion empty");
    }
}

#[test]
fn exit_code_propagates_failure() {
    let req = r#"{"steps":[{"cmd":"exit 7"}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 1, "batch failure must exit 1");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"][0]["exit"], serde_json::json!(7));
}

#[test]
fn timeout_reports_structured() {
    let req = r#"{"steps":[{"cmd":"sleep 5","timeout_ms":200}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let r = &v["results"][0];
    assert_eq!(r["exit"], serde_json::json!(-1));
    assert_eq!(r["timeout"], serde_json::json!(true));
}
