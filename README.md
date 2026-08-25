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
- Cursor: the official account usage CSV export, using the existing local Cursor
  sign-in session; rows in each valid export replace that account's returned
  time window while older history is retained
- Qoder and Qoder CN: the validated `SharedClientCache/cache/db/local.db`
  assistant-token schema, with cached input separated from ordinary input
- TRAE SOLO CN: the official per-session usage API for a rolling 30-day window;
  this remote read is disabled until explicitly enabled under Settings → Data & Sync

The Limits page also reads account quota from the existing local login state for
Claude Code, Codex, Cursor, Qoder/Qoder CN, and Grok. TRAE SOLO contributes its
local entitlement and Fast Request allowance; its local state does not expose
current usage, so LLMeter does not invent a percentage. Cursor's official export
provides account-level token events but does not identify ordinary local chat
sessions. TRAE SOLO international still exposes no readable per-request token
log; TRAE SOLO CN installation and sign-in are detected locally, and its JWT is
sent only to TRAE's pinned official API after the user enables token collection.
Remote Cursor and TRAE snapshots are account-scoped, so switching accounts does
not overwrite another account's recorded usage.

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
The rescan subcommand clears usage and cursors only for providers that can be
fully rebuilt from local sources. Remote snapshot history is retained and then
refreshed, so data outside a provider's API window is not discarded. Deleted
local agent history cannot be recovered by a rescan.
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
