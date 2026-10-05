# OptChat

A native Rust + egui implementation of [Victor Taelin's OptChat specification](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449), fetched October 5, 2026 (gist revision `f51fe5c910427fd6f384d22823140b1693c76207`). The complete source specification is in [SPEC.md](SPEC.md).

This replaces the earlier OptMem `memo` CLI. The package is now `optchat`, in `optchat-rs`; it does not read or modify legacy OptMem stores.

## Run

### macOS application

Run `./build.sh` to build a locally signed app for this Mac at `target/macos/OptChat.app`. Run `./build.sh --install` to also install it in `~/Applications`, then open **OptChat** in Finder. Rust and Xcode Command Line Tools are required for building. The script refuses to overwrite an existing installed app; move the old bundle aside before reinstalling.

The installed app keeps using this checkout's `chat/` directory, preserving your existing history. Keep that directory in place; `OPTCHAT_DIR` can override it when launching from a terminal. API keys are configured inside Settings. This is an ad-hoc signed local build, not a notarized distribution for other Macs.

### From a terminal

Requires Rust 1.95 or newer and an Anthropic or DeepSeek API key. Launch the app, then open **Settings**, select your provider, enter its API key, and click **Save and apply**:

```sh
cd /Users/benj/workspace/main/optchat-rs
cargo run --locked
```

Keys are saved per provider in macOS Keychain, Windows Credential Manager, or Linux Secret Service. Saved keys take precedence over environment variables and load on subsequent launches. Leave the key field blank to keep the existing key; enter a replacement to update it. Keys are never stored in chat files. A locked or unavailable credential store produces an error without replacing the active client. Linux requires a running Secret Service.

To start with DeepSeek selected, run:

```sh
OPTCHAT_PROVIDER=deepseek cargo run --locked
```

DeepSeek uses its official [Anthropic-compatible endpoint](https://api-docs.deepseek.com/guides/anthropic_api/), including streaming and memory tool calls. Both model roles default to `deepseek-flash`; override them independently with `OPTCHAT_MODEL` and `OPTCHAT_COMPACTOR` (for example, `deepseek-v4-pro` for the master and `deepseek-flash` for compaction).

Without a key, history browsing, export, import, and free summaries work. Model requests display a connection error. No credentials are written to chat files.

| Environment variable | Default |
| --- | --- |
| `OPTCHAT_DIR` | This project's `chat/` directory |
| `OPTCHAT_PROVIDER` | `anthropic`; also accepts `deepseek` |
| `ANTHROPIC_API_KEY` | Used only for Anthropic |
| `DEEPSEEK_API_KEY` | Used only for DeepSeek |
| `OPTCHAT_MODEL` | `claude-sonnet-4-6` for Anthropic; `deepseek-flash` for DeepSeek |
| `OPTCHAT_COMPACTOR` | Same provider default as the master; independently configurable |
| `OPTCHAT_INSTRUCTIONS` | `AGENTS.md` in the launch directory, if present |
| `OPTCHAT_ENDPOINT` | `https://api.anthropic.com/v1/messages` or `https://api.deepseek.com/anthropic/v1/messages` |

Anthropic uses adaptive thinking and medium effort. DeepSeek uses enabled thinking and high effort (DeepSeek maps medium to high). Choose models supporting these options. Settings lets you select a provider and change both models between turns; selecting a provider fills its model defaults, and **Save and apply** activates the selection and any new key without a restart. Provider and model selections remain session-only; launch environment overrides still work. Background compaction already in flight completes with its original provider and key; subsequent requests use the updated settings.

The endpoint override is intended for a trusted compatible gateway or local testing; requests include your API key. Switching providers in Settings resets the endpoint to that provider's official URL. Changing only model names preserves the current endpoint.

Send with the button or Command+Enter. Messages submitted during a tool loop arrive at its next tool boundary; messages arriving after the last boundary start another fresh turn. Stop preserves waiting input in the log without generating a reply. Closing the window gracefully drains completed entries and saves pending input.

Memory shows the current view. Select a range to inspect its two children, then drill down to the verbatim message. Settings can export a standalone HTML page with the view, all messages, and every tree level, including ranges, timestamps, and byte sizes. Import accepts UTF-8 text, one non-empty line per `note`, appended with new global IDs. Imports are permanent and intentionally not deduplicated.

## Spec behavior

- Daily append-only `main/YYYY-MM-DD.jsonl` and `tree/YYYY-MM-DD.jsonl`; each append is flushed and synced before becoming visible. Unix directory entries are synced too.
- One process owns the store for its lifetime. A portable OS advisory file lock replaces the reference Unix socket: the OS releases it after a crash, with no PID files or timeout takeover. Keep the directory on a local filesystem with working advisory locks.
- Invalid JSON lines are reported and skipped. A missing trailing newline is appended before subsequent writes. Invalid IDs or sizes cause an explicit load failure rather than silently reassigning message identities.
- Pure binary summaries: 512-byte target, free verbatim nodes when the source fits, every parent from exactly two children. Original messages are never deleted or edited.
- Up to eight background jobs, ordered through the first unfinished view line. Failed model calls retry after ten seconds; the first failure is shown. Compaction gets the complete source plus preceding summarized context, without address labels.
- The verbatim COMPACT, MASTER, and VIEW_DOC prompts are included in source. Compression has a 512-byte scale example, five attempts in the same conversation, UTF-8-safe size feedback, and retains the shortest result even if slightly over target.
- A 128,000-byte summary-text budget; append and merge the most-due available sibling pair, never split or recompute a new tiling per turn. Startup reconstructs the view by folding the log in order.
- Fresh model conversation per turn, after summaries settle. The view is captured before the new user messages are logged. No model call receives placeholder or truncated unsummarized history.
- `zoom(id,n)` and `date(id)` tools; tool results are capped at 30,000 Unicode characters with head, tail, and an omission count. Tool calls and completed output blocks are logged as they finish.
- Streamed thoughts remain in the UI only. Complete vendor content blocks, including thinking signatures, are retained unchanged within the active API conversation and discarded between turns.
- Anthropic view cache marks at line ends preceding 50k/80k/100k characters, plus automatic request-end caching. DeepSeek requests omit these markers because it manages caching automatically; the stable view prefix is preserved. No renewal pings. API usage counters are visible in the status area; unavailable counters show `—`, and cache misses are not mislabeled as cache writes.

## Explicit scope choices

The requested egui desktop interface replaces the spec's plain terminal interface. It runs locally while the app is open; remote attachment and an always-on server are not included. Providers are Anthropic and DeepSeek; OpenAI-specific transport is not implemented. The model has the memory tools, not shell, filesystem execution, web, or computer-use tools. Optional subagents are omitted. Files are durable after each entry; automatic Git commits and remote backups are not performed. Back up the complete chat directory yourself.

These are not claims of perfect recall: summaries can omit retrieval clues even though original text remains intact. Live model summary quality and actual vendor cache-hit rates require testing with your credentials. Local fixtures verify transport, request layout, tool continuity, and storage behavior without paid calls.

## Verify

```sh
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
```

Tests cover durable reloads, locking, torn tails, write failures, ordered tree construction, view coarsening, zoom validation, UTF-8/cache boundaries, HTML escaping, streaming blocks, fresh turns, tool results, signed reasoning preservation without logging, compression retries, and canceled waits.
Provider tests exercise both Anthropic and DeepSeek request policies, streamed tool loops, compaction retries, missing-key errors, and usage reporting through local HTTP fixtures. They do not establish live provider compatibility or summary quality.

Credential tests use an isolated in-memory store to verify reloads, provider isolation, failed replacements, and the key sent on subsequent requests. The optional `cargo test --locked native_store_round_trip -- --ignored` checks the OS credential store with disposable test entries and removes them afterward.
