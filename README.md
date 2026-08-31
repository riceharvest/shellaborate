# shellaborate
Batch shell: terminal + git + gh + execute_code + process in one DAG (terminal->terminal 71.9%, 99k). Pure Rust.

Why: 71.9% terminal->terminal loops collapse. Runs git/gh/shell DAG concurrently.

## Bigram evidence (from hermes state.db ~283k tool calls)
See `cargo test` and `src/lib.rs` for batch API. Pure Rust, tokio.

## Usage
```bash
cargo build --release
echo '{"items":[{}]}' | ./target/release/shellaborate --input -
```
```bash
cargo test
```
