# shellaborate

Batch shell for AI agents: one call replaces N terminal calls. Runs
`shell`/`git`/`gh` commands as a DAG with dependency ordering, bounded
concurrency, timeouts, and structured JSON results — no output scraping.

Why: hermes telemetry (~283k tool calls) shows terminal->terminal is the
biggest chain (71.9%, 99k), and most "terminals" are just
`cd DIR && cmd && cmd`. One shellaborate call collapses the whole sequence.

Pure Rust (edition 2024, MSRV 1.85). No network at run time.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/riceharvest/shellaborate/main/install.sh | sh
```

Or from source:

```sh
cargo install --path .
```

Self-update (checksum-verified, from GitHub Releases):

```sh
shellaborate --update
```

## Usage

Pipe a JSON `BatchRequest` to stdin, get JSON on stdout.

Linear shorthand (steps run in order, each depends on the previous):

```sh
echo '{"steps":[
  {"cmd":"cargo check"},
  {"cmd":"cargo test 2>&1 | tail -20"}
]}' | shellaborate
```

DAG with parallel branches (default concurrency 6):

```sh
echo '{"commands":[
  {"id":"fmt","cmd":"cargo fmt --check"},
  {"id":"clippy","cmd":"cargo clippy --all-targets"},
  {"id":"audit","cmd":"cargo audit"},
  {"id":"report","cmd":"echo done"}
],"dag":[
  {"from":"fmt","to":"report"},
  {"from":"clippy","to":"report"},
  {"from":"audit","to":"report"}
]}' | shellaborate
```

`cd` prefixes are normalized into the step's working directory:
`"cmd":"cd crates/x && cargo test"` runs `cargo test` in `crates/x`.

### Request fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `steps` | `[Step]` | — | Linear chain (each depends on the previous) |
| `commands` | `[Step]` | — | DAG nodes; independent ones run concurrently |
| `dag` | `[{from,to}]` | — | Extra edges: `to` runs after `from` completes |
| `concurrency` | int | 6 | Max parallel branches (cap 50) |
| `timeout_ms` | int | 30000 | Default per-step timeout |
| `keep_going` | bool | false | Run independent branches after a failure |
| `allow_dangerous` | bool | false | Permit denylisted commands |

`Step`: `{id?, cmd, cwd?, env?, timeout_ms?, shell?}` — `shell` is `sh`
(default) or `bash`; `cmd` is the shell program text, no extra escaping is
applied. Max 50 commands per request.

### Response

```json
{
  "ok": true,
  "results": [
    {"id":"a","cmd":"echo a","kind":"shell","exit":0,"stdout":"a\n",
     "elapsed_ms":1,"cwd":"/repo"}
  ],
  "skipped": [],
  "elapsed_ms": 4
}
```

- `ok` is true iff every executed step exited 0 and nothing was skipped.
- `kind` classifies the first word: `shell` / `git` / `gh`.
- `git diff --stat` / `git show --stat` steps additionally get a parsed
  `git_stat` array: `{"path":"src/lib.rs","changes":12,"bar":"+++++"}`.
- `gh` auth failures surface a clean `error` ("run `gh auth login`") instead
  of raw stderr soup.
- `skipped` lists steps never started (dependency failed or run cancelled).
- `exit` is `-1` on timeout/spawn failure; `timeout:true` marks the former.

## CLI flags

```
--input FILE    Read request from FILE instead of stdin ("-" = stdin)
--dry-run       Print the execution plan (id/cmd/deps) without running
--output json   Single JSON object (default)
--output ndjson One JSON line per finished step, then a summary line
--pretty        Pretty-print JSON
--allow-dangerous  Permit denylisted commands
--emit TARGET   Emit hermes-tool.json | man | bash | zsh | fish and exit
--update        Self-update from GitHub Releases
```

Exit codes: `0` all steps succeeded, `1` batch ran but some step failed
(nonzero exit or timeout), `2` request error (bad JSON, cap, cycle,
denylisted command).

## Hermes integration

Install, then register the tool. `shellaborate --emit hermes-tool.json`
prints a JSON schema describing the request/response contract; register it
with your tool config:

```json
{
  "name": "shellaborate",
  "command": "shellaborate",
  "input": "stdin",
  "format": "json"
}
```

For long runs, use `--output ndjson` to see each step land as it finishes.

## Security model

shellaborate executes shell text — that IS the injection surface, so the
tool is built around bounding the blast radius:

- No double-escaping: `cmd` is passed verbatim as the single argument to
  `sh -c`/`bash -c`. The caller composes shell syntax; the tool never
  re-interprets or concatenates it.
- Environment deny-by-default: children get only `PATH`, `HOME`,
  `LANG`-ish, and `TMPDIR`-ish vars from the parent, plus what you pass in
  `step.env`. Ambient tokens (GH_TOKEN, AWS keys, ...) do not leak into
  steps unless explicitly injected.
- Output caps: stdout/stderr are capped at 5 MiB per stream (kept draining
  so children never block); `stdout_truncated`/`stderr_truncated` flag it.
  No OOM from a runaway `cat` of a huge file.
- Timeouts kill the whole process group (SIGKILL on the child's pgid), so
  grandchildren (e.g. `sh -c "sleep 5 && ..."` orphaning `sleep`) cannot
  outlive the deadline or hold the pipes open.
- Concurrency is capped (`concurrency`, hard max 50 commands per batch) —
  fork bombs are bounded by the OS, not the batch; literal fork-bomb strings
  and destructive commands are denylisted.
- Denylist: `rm -rf /`, `shutdown`/`reboot`/`halt`/`poweroff`, `init 0|6`,
  `mkfs*`, `dd of=/dev/...`, fork bombs. Override per invocation with
  `--allow-dangerous` (operator choice, surfaced in the response).
- Path traversal: `cwd` rejects any `..` component.
- Secrets in output: lines containing token/key-shaped strings (`ghp_`,
  `github_pat_`, `xoxb-`, `Bearer `, `*_TOKEN`, `API_KEY`, ...) are masked
  to `§§§§§§§§` before they reach the response.

Nothing is executed on a network basis at run time; `--update` is the only
networked path and verifies SHA256 checksums.

## Testing

```sh
cargo test            # 21 unit + 12 integration (real binary, no network)
cargo clippy --all-targets
```

## License

MIT OR Apache-2.0
