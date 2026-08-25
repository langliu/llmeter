# LLMeter

Local-first AI coding usage tracker for macOS.

LLMeter reads the session logs produced by local coding agents, converts token
metadata into a unified UsageEvent, and stores only local usage aggregates in
SQLite. It does not proxy model traffic, require an API key, or upload data.

## Architecture

~~~text
Agent Session Logs
        ↓
Incremental Provider Adapters
        ↓
UsageEvent
        ↓
SQLite
        ↓
GPUI
~~~

The core, storage, collector, and app crates are intentionally separated.
Provider parsers never update UI state directly. The collector uses an initial
scan, filesystem events, a debounced single-flight sync, and a five-minute
safety rescan.

## Supported providers

- Codex CLI: ~/.codex/sessions/**/*.jsonl
- Claude Code: ~/.claude/projects/**/*.jsonl
- OpenCode: JSONL plus the validated SQLite session/session_v2 token schema;
  unknown SQLite schemas are explicitly marked unsupported
- pi: ~/.pi/agent/sessions
- Oh My Pi: ~/.omp/agent/sessions
- Zed: the validated threads SQLite database on macOS and Linux; both JSON and
  zstd-compressed thread payloads are supported
- Grok: ~/.grok/sessions/**/updates.jsonl (or $GROK_HOME/sessions), including
  cache-token breakdowns and the exact cost reported by Grok Build
- Hermes Agent: $HERMES_HOME/state.db (default ~/.hermes/state.db) plus local
  profile databases, using per-model usage with aggregate-session reconciliation

Codex last_token_usage snapshots are treated as direct deltas when present.
Other cumulative snapshots go through a per-session cumulative tracker, so
repeated scans do not add the same counter again.

## Privacy

LLMeter never stores prompts, responses, reasoning content, chat messages, file
contents, or source code. Parsers inspect JSON objects only to find usage and
metadata; malformed-line warnings include the provider, path, and byte
position, never the raw session line.

By default the database and local state live under:

~/Library/Application Support/LLMeter/

For development and tests, use LLMETER_DATA_DIR:

~~~sh
LLMETER_DATA_DIR=/tmp/llmeter
~~~

## Development

~~~sh
cargo run -p llmeter-app
cargo run -p llmeter-app -- notify --provider codex
cargo run -p llmeter-app -- rescan
cargo run -p llmeter-app -- hook status --provider codex
cargo run -p llmeter-app -- hook install --provider codex
~~~

Build the macOS application bundle (requires
[`cargo-bundle`](https://github.com/burtonageo/cargo-bundle)):

~~~sh
cargo install cargo-bundle
cargo bundle --release -p llmeter-app
open target/release/bundle/osx/LLMeter.app
~~~

The notify subcommand is intentionally lightweight: it writes a local signal
and exits. The running app performs the actual incremental parse.
The rescan subcommand explicitly clears derived usage and cursors, then
rebuilds them from session files that still exist. Deleted agent history cannot
be recovered by a rescan.
Hook installation is opt-in; existing Codex notify configuration is treated as
a conflict, and Claude hooks carry an LLMeter marker so uninstall only removes
the managed entry.

Run the verification suite:

~~~sh
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
~~~

## Current implementation decisions

- GPUI is pinned to the Zed git workspace crates (`gpui` / `gpui_platform`)
  with `runtime_shaders`, so building does not require the legacy xcrun Metal
  shader toolchain at compile time.
- The first version is a single process. The collector/event boundary leaves
  room for a future daemon and IPC split.
- Hook installation is opt-in, backs up existing configuration, and refuses
  to overwrite an existing Codex notify entry.
- OpenCode SQLite support is limited to the validated session/session_v2 token
  columns; other schemas are reported as unsupported instead of guessed.
- The MVP focuses on the main window, local collection, aggregation, and
  privacy. Menu-bar integration and a full settings action surface remain
  follow-up work.
