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

/// Spawn the installed/debug binary in a given cwd with piped stdio.
fn spawn_in_dir(cwd: &std::path::Path, args: &[&str]) -> std::process::Child {
    Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd)
        .spawn()
        .expect("spawn shellaborate")
}

fn write_stdin(child: &mut std::process::Child, stdin: &str) {
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
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
    let mut child = spawn_in_dir(repo, &[]);
    write_stdin(&mut child, req);
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
    // exit non-zero, emit valid JSON with ok=false, and never panic or dump a
    // stacktrace. Loopback only — no external network in tests.
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

// ---------- adversarial audit (5 probes) ----------

#[test]
fn audit_forkbomb_denied_by_denylist() {
    // Literal forkbomb in several spacing variants must be denied pre-exec.
    for cmd in [
        ":(){ :|:& };:",
        ":(){:|:&};:",
        ":(){:|: &};:",
        "echo x && bash -c \":(){ :|:& };:\"",
    ] {
        let req = serde_json::json!({ "steps": [{ "cmd": cmd }] }).to_string();
        let (code, out, _) = run_cli(&[], &req);
        assert_eq!(code, 2, "forkbomb {cmd:?} must be denied: {out}");
        assert!(out.contains("forkbomb"), "reason surfaced: {out}");
    }
    // --allow-dangerous override accepts the request (shell may then reject
    // the syntax, but the tool itself must not refuse).
    let req = r#"{"steps":[{"cmd":":(){ :|:& };:","timeout_ms":3000}]}"#;
    let (code, _, _) = run_cli(&["--allow-dangerous"], req);
    assert_ne!(code, 2, "override must bypass the denylist");
}

#[test]
fn audit_secret_env_does_not_leak() {
    // GH_TOKEN injected via step.env then echoed: output must be masked.
    let req =
        r#"{"steps":[{"cmd":"echo token=$GH_TOKEN","env":{"GH_TOKEN":"ghp_SUPERSECRET123456"}}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 0);
    assert!(!out.contains("ghp_SUPERSECRET"), "secret leaked: {out}");
    assert!(out.contains("§§§§§§§§"), "mask marker present: {out}");
    // Also as command text in output capture.
    let req = r#"{"steps":[{"cmd":"env | grep -c GH_TOKEN || true; echo GH_TOKEN=zzz"}]}"#;
    let (_, out, _) = run_cli(&[], req);
    assert!(out.contains("§§§§§§§§"), "echoed key name masked: {out}");
}

#[test]
fn audit_secret_env_is_sanitized() {
    // Ambient env is NOT inherited: a secret in the parent environment must
    // not reach the step unless explicitly passed via step.env.
    let req = r#"{"steps":[{"cmd":"printenv GH_TOKEN | wc -c"}]}"#;
    let mut child = Command::new(bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_TOKEN", "ghp_AMBIENTSECRET000000")
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
    assert!(
        !stdout.contains("ghp_AMBIENTSECRET"),
        "ambient leak: {stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // wc -c on empty input prints 0 (no newline) => "0\n"
    assert_eq!(v["results"][0]["stdout"], serde_json::json!("0\n"), "{v}");
}

#[test]
fn audit_cwd_escape_rejected_both_ways() {
    // Explicit cwd with traversal.
    let req = r#"{"steps":[{"cmd":"pwd","cwd":"../../"}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 1);
    assert!(out.contains("path traversal rejected"), "{out}");
    // Traversal smuggled through a cd prefix (normalized into cwd).
    let req = r#"{"steps":[{"cmd":"cd ../../../etc && head -1 passwd"}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 1);
    assert!(out.contains("path traversal rejected"), "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["results"][0]["stdout"].is_null(), "never executed: {v}");
}

#[test]
fn audit_output_dos_capped() {
    // 10MB flood must be capped at 5MiB + one read chunk, flagged, and the
    // process must stay small (no OOM).
    let req = r#"{"steps":[{"cmd":"yes | head -c 10M","timeout_ms":15000}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 0, "the command itself succeeds: {out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let r = &v["results"][0];
    assert_eq!(r["stdout_truncated"], serde_json::json!(true), "{out}");
    let len = r["stdout"].as_str().unwrap().len();
    assert!(len <= 5 * 1024 * 1024 + 8192, "captured {len} bytes");
}

#[test]
fn audit_cycle_detected_exit2() {
    let req = r#"{"commands":[{"id":"a","cmd":"echo a"},{"id":"b","cmd":"echo b"}],"dag":[{"from":"a","to":"b"},{"from":"b","to":"a"}]}"#;
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["error"].as_str().unwrap().contains("cycle"), "{out}");
    assert_eq!(v["results"].as_array().unwrap().len(), 0, "nothing ran");
}

// ---------- task 2 qol flags ----------

#[test]
fn list_dag_prints_dependency_graph() {
    let req = r#"{"commands":[{"id":"a","cmd":"echo a"},{"id":"b","cmd":"echo b"},{"id":"c","cmd":"echo c"}],"dag":[{"from":"a","to":"b"},{"from":"a","to":"c"}]}"#;
    let (code, out, _) = run_cli(&["--list-dag"], req);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "a\techo a\nb\techo b\t[after: a]\nc\techo c\t[after: a]\n"
    );
}

#[test]
fn timings_flag_adds_summary_table() {
    let req = r#"{"steps":[{"cmd":"echo a"},{"cmd":"sleep 0.05 && echo b"}]}"#;
    let (code, out, _) = run_cli(&["--timings"], req);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let timings = v["timings"].as_array().expect("timings field present");
    assert_eq!(timings.len(), 2);
    assert!(timings[0]["elapsed_ms"].is_u64());
    // Without the flag the field must be absent.
    let (code, out, _) = run_cli(&[], req);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v.get("timings").is_none());
}

#[test]
fn hermes_tool_json_describes_request_precisely() {
    let (code, out, _) = run_cli(&["--emit", "hermes-tool.json"], "");
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let input = &v["input"]["properties"];
    // commands: capped at 50
    assert_eq!(input["commands"]["maxItems"], serde_json::json!(50));
    // concurrency: documented bounds and default
    assert_eq!(input["concurrency"]["maximum"], serde_json::json!(50));
    assert_eq!(input["concurrency"]["default"], serde_json::json!(6));
    // timeout: documented default field + process-group kill semantics
    let t = input["timeout_ms"]["description"].as_str().unwrap();
    assert!(t.contains("SIGKILL"), "{t}");
    assert_eq!(input["timeout_ms"]["default"], serde_json::json!(30000));
    // dag: cycle/unknown-id rejection documented
    let d = input["dag"]["description"].as_str().unwrap();
    assert!(d.contains("Cycle"), "{d}");
    // allow_dangerous: denylist enumerated
    let a = input["allow_dangerous"]["description"].as_str().unwrap();
    assert!(a.contains("rm -rf /"), "{a}");
}

// ---------- artifact capture (option b) ----------

#[test]
fn capture_flag_collects_artifacts() {
    // Step writes two files; --capture collects them with sha256 + inline.
    let dir = tempfile::tempdir().unwrap();
    let req = r#"{"steps":[{"cmd":"echo build ok > out.txt && echo v1 > out.ver"}]}"#;
    let mut child = spawn_in_dir(
        dir.path(),
        &["--capture", "out.txt", "--capture", "out.ver"],
    );
    write_stdin(&mut child, req);
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arts = v["artifacts"].as_array().expect("artifacts array present");
    assert_eq!(arts.len(), 2);
    let paths: Vec<&str> = arts.iter().map(|a| a["path"].as_str().unwrap()).collect();
    assert!(
        paths.contains(&"out.txt") && paths.contains(&"out.ver"),
        "{paths:?}"
    );
    let txt = arts.iter().find(|a| a["path"] == "out.txt").unwrap();
    assert_eq!(txt["content"], serde_json::json!("build ok\n"));
    assert_eq!(txt["inlined"], serde_json::json!(true));
    assert_eq!(txt["sha256"].as_str().unwrap().len(), 64);
}

#[test]
fn capture_json_field_and_cli_flag_merge() {
    // JSON capture spec + CLI --capture flag both apply; dedupe handles overlap.
    let dir = tempfile::tempdir().unwrap();
    let req =
        r#"{"steps":[{"cmd":"echo a > f1.txt && echo b > f2.txt"}],"capture":[{"path":"f1.txt"}]}"#;
    let mut child = spawn_in_dir(dir.path(), &["--capture", "f2.txt"]);
    write_stdin(&mut child, req);
    let out = child.wait_with_output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let arts = v["artifacts"].as_array().unwrap();
    assert_eq!(arts.len(), 2, "both files captured: {v}");
}

#[test]
fn capture_traversal_via_cli_rejected() {
    let req = r#"{"steps":[{"cmd":"echo x"}]}"#;
    let (code, out, _) = run_cli(&["--capture", "../outside.txt"], req);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("traversal"), "{out}");
}

#[test]
fn capture_works_on_failed_batch() {
    // The whole point: failed run still returns the log for debugging.
    let dir = tempfile::tempdir().unwrap();
    let req = r#"{"steps":[{"cmd":"echo step1 > ok.log"},{"cmd":"exit 5"}],"capture":[{"path":"ok.log"}]}"#;
    let mut child = spawn_in_dir(dir.path(), &[]);
    write_stdin(&mut child, req);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1), "step failure exits 1");
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["ok"], serde_json::json!(false));
    let arts = v["artifacts"]
        .as_array()
        .expect("artifacts present on failed batch");
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0]["content"], serde_json::json!("step1\n"));
}
