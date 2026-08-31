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

Both honor the overrides `SHELLABORATE_INSTALL_DIR` (default
`~/.local/bin`, cargo puts it in `~/.cargo/bin`) and `SHELLABORATE_VERSION`
(pin a release). Release archives contain the binary, README, LICENSE-MIT,
and a generated `hermes-tool.json`.

Self-update (checksum-verified, from GitHub Releases):

```sh
shellaborate --update
```

With no published release this exits 1 with a clean HTTP 404 message — not
a stacktrace.

## Usage

Pipe a JSON `BatchRequest` to stdin, get JSON on stdout.

### Demo DAG

`b` depends on `a`; `c` is an independent branch and runs alongside `a`.

```sh
echo '{"commands":[
  {"id":"a","cmd":"echo a"},
  {"id":"b","cmd":"echo b depends on a"},
  {"id":"c","cmd":"echo parallel branch"}
],"dag":[{"from":"a","to":"b"}]}' | shellaborate --timings
```

Actual output:

```json
{
  "elapsed_ms": 4,
  "ok": true,
  "results": [
    {"cmd": "echo a", "cwd": "/repo", "elapsed_ms": 2, "exit": 0, "id": "a",
     "kind": "shell", "stdout": "a\n", "stdout_truncated": false},
    {"cmd": "echo b depends on a", "elapsed_ms": 1, "exit": 0, "id": "b",
     "kind": "shell", "stdout": "b depends on a\n"},
    {"cmd": "echo parallel branch", "elapsed_ms": 2, "exit": 0, "id": "c",
     "kind": "shell", "stdout": "parallel branch\n"}
  ],
  "skipped": [],
  "timings": [
    {"elapsed_ms": 2, "id": "a"},
    {"elapsed_ms": 1, "id": "b"},
    {"elapsed_ms": 2, "id": "c"}
  ]
}
```

Inspect the plan without executing:

```sh
$ echo '{"commands":[{"id":"a","cmd":"echo a"},{"id":"b","cmd":"echo b"},
  {"id":"c","cmd":"echo c"}],"dag":[{"from":"a","to":"b"},{"from":"a","to":"c"}]}' \
  | shellaborate --list-dag
a	echo a
b	echo b	[after: a]
c	echo c	[after: a]
```

Linear shorthand (steps run in order, each depends on the previous):

```sh
echo '{"steps":[
  {"cmd":"cargo check"},
  {"cmd":"cargo test 2>&1 | tail -20"}
]}' | shellaborate
```

`cd` prefixes are normalized into the step's working directory:
`"cmd":"cd crates/x && cargo test"` runs `cargo test` in `crates/x`.

### Request fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `steps` | `[Step]` | — | Linear chain (each depends on the previous) |
| `commands` | `[Step]` | — | DAG nodes; independent ones run concurrently |
| `dag` | `[{from,to}]` | — | Edges: `to` runs only after `from` completes |
| `concurrency` | int | 6 | Max branches in parallel (min 1, max 50) |
| `timeout_ms` | int | 30000 | Default per-step timeout |
| `keep_going` | bool | false | Run independent branches after a failure |
| `allow_dangerous` | bool | false | Permit denylisted commands |

`Step`: `{id?, cmd, cwd?, env?, timeout_ms?, shell?}` — `shell` is `sh`
(default) or `bash`; `cmd` is the shell program text, no extra escaping is
applied. Max 50 commands per request. `to` runs after `from` completes
regardless of `from`'s exit code; failure semantics are handled by
`keep_going` and the scheduler (below).

### Response

- `ok` is true iff every executed step exited 0 and nothing was skipped.
- `kind` classifies the first word: `shell` / `git` / `gh`.
- `git diff --stat` / `git show --stat` steps additionally get a parsed
  `git_stat` array: `{"path":"src/lib.rs","changes":12,"bar":"+++++"}`.
- `gh` auth failures surface a clean `error` ("run `gh auth login`") instead
  of raw stderr soup.
- `skipped` lists steps never started (dependency failed or run cancelled).
- `exit` is `-1` on timeout/spawn failure; `timeout:true` marks the former.
- `--timings` adds the per-step `timings` table shown above.

## CLI flags

```
--input FILE    Read request from FILE instead of stdin ("-" = stdin)
--dry-run       Print the execution plan (id/cmd/deps) without running
--list-dag      Print the dependency graph as text (id, cmd, [after: ...])
--timings       Add per-step elapsed_ms table to the summary
--output json   Single JSON object (default)
--output ndjson One JSON line per finished step, then a summary line
--pretty        Pretty-print JSON
--allow-dangerous  Permit denylisted commands
--emit TARGET   Emit hermes-tool.json | man | bash | zsh | fish and exit
--update        Self-update from GitHub Releases
```

Exit codes: `0` all steps succeeded, `1` batch ran but some step failed
(nonzero exit, timeout, or a step error like rejected cwd), `2` request
error (bad JSON, >50 commands, cycle, unknown dep id, denylisted command).

## Hermes integration

Install, then register the tool. `shellaborate --emit hermes-tool.json`
prints a JSON schema describing the request/response contract
(commands/dag/concurrency/timeout/allow_dangerous with exact bounds and
semantics); register it with your tool config:

```json
{
  "name": "shellaborate",
  "command": "shellaborate",
  "input": "stdin",
  "format": "json"
}
```

For long runs, use `--output ndjson` to see each step land as it finishes.

Note for agent harnesses: if your `terminal` wrapper wedges (stale cwd, a
dead persistent shell that fails every call with exit 126), `execute_code`
with an explicit `workdir` is the working fallback — that combination is
how shellaborate's own test loops ran.

## Security model

shellaborate executes shell text — that IS the injection surface, so the
tool is built around bounding the blast radius. Each of these is covered by
a test in `tests/cli.rs` (the `audit_*` tests):

- No double-escaping: `cmd` is passed verbatim as the single argument to
  `sh -c`/`bash -c`. The caller composes shell syntax; the tool never
  re-interprets or concatenates it.
- Environment deny-by-default: children get only `PATH`, `HOME`,
  `LANG`-ish, and `TMPDIR`-ish vars from the parent, plus what you pass in
  `step.env`. A `GH_TOKEN` sitting in the agent's environment does not
  reach steps (`printenv GH_TOKEN` comes back empty) unless explicitly
  injected — and injected secrets that get echoed are masked.
- Output caps: stdout/stderr are capped at 5 MiB per stream (kept draining
  so children never block); `stdout_truncated`/`stderr_truncated` flag it.
  A 10 MB `yes` flood returns 5 MiB + one 8 KiB chunk, flagged. No OOM.
- Timeouts kill the whole process group (SIGKILL on the child's pgid), so
  grandchildren (e.g. `sh -c "sleep 5 && ..."` orphaning `sleep`) cannot
  outlive the deadline or hold the pipes open.
- Concurrency is bounded: max 50 commands per batch, parallel children
  capped by `concurrency` (6x `sleep 0.2` at concurrency 6 completes in
  ~205ms, not 1200ms — the semaphore, not wishful thinking, does it).
- Denylist: `rm -rf /`, `shutdown`/`reboot`/`halt`/`poweroff`, `init 0|6`,
  `mkfs*`, `dd of=/dev/...`, and forkbombs (`:(){ :|:& };:` caught in all
  spacing variants, including nested inside `bash -c "..."` — the signature
  is matched shape-wise, not by exact string). Denied requests exit 2
  before anything runs. Override with `--allow-dangerous` (operator
  choice); the shell itself remains the final judge of what executes.
- Path traversal: `cwd` rejects any `..` component — and so does a `..`
  smuggled in through a `cd ../../../etc && ...` prefix, before execution.
- Secrets in output: lines containing token/key-shaped strings (`ghp_`,
  `github_pat_`, `xoxb-`, `Bearer `, `*_TOKEN`, `API_KEY`, ...) are masked
  to `§§§§§§§§` before they reach the response.
- Cycles: `A->B->A` is rejected with exit 2 and an empty `results` array.

Nothing is executed on a network basis at run time; `--update` is the only
networked path and verifies SHA256 checksums.

## Testing

```sh
cargo test            # 21 unit + 21 integration (real binary, no network)
cargo clippy --all-targets
```

The integration suite includes the five adversarial probes: forkbomb,
secret leak (injected and ambient), cwd escape (explicit and cd-prefixed),
output DoS, and dependency cycle.

## License

MIT OR Apache-2.0
