# OptChat specification audit

Audited 2026-10-05 against [Victor Taelin's specification](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449), revision `f51fe5c910427fd6f384d22823140b1693c76207`. The GitHub Gist API reports this as the latest revision; its complete `optchat.md` content is byte-identical to local `SPEC.md`.

## Result

The core memory architecture follows the specification. This is not a complete implementation of every operational feature in the document. The native egui interface and DeepSeek provider are requested adaptations. Remote operation, general agent tools, historical session importing, and automated backup remain absent. Live provider behavior has not been verified with paid requests.

The audit found and fixed missing metadata in the HTML export's current-view section: each view entry now includes its range, byte size, and time span, alongside the original rendered view. A regression test failed before the fix and passes afterward.

## Requirement comparison

| Spec | Implementation and evidence | Assessment |
| --- | --- | --- |
| §1 constants | `lib.rs`: NODE 512, VIEW 128,000, JOBS 8, TRIES 5, CAP 30,000. Runtime retry is 10 seconds; cache marks are 50k/80k/100k characters. | Matches |
| §2 canonical log | `memory.rs`: daily local-date JSONL, permanent contiguous IDs, kinds user/talk/tool/echo/note, RFC3339 dates, UTF-8 source byte sizes. Original records are retained. | Matches for supported stores |
| §2 durability | `Store::append`: one write, flush, fsync before publishing to in-memory state; short writes stop the worker. Unix directory entries are synced. Persistence failure test verifies records are not published on failed writes. | Matches; physical power-loss behavior not exercised |
| §2 one writer | Lifetime OS advisory file lock; second opener fails and dropping the first allows reopening. | Equivalent mechanism; reference uses a Unix socket |
| §2 torn records | Invalid JSON is reported and skipped; missing final newline is appended and synced. Torn-tail recovery is tested. | Matches ordinary torn-tail recovery; see corruption limitation below |
| §2 reasoning | Streaming thoughts are displayed but excluded from ROOT. Signed reasoning remains in the active API conversation. Tests cover both providers. | Matches |
| §3 binary tree | Level 0 summarizes exactly one source; parents use exactly two children. Sources fitting 512 bytes become free nodes. Addressing uses first message plus power-of-two width. | Matches |
| §4 scheduling | `Memory::eligible` enforces built sources and the first-unbuilt-view boundary. Runtime limits active jobs to eight, retries after ten seconds, and reports the first failure until success. | Matches scheduling intent; retry delay does not occupy an active job slot |
| §4 compactor input | Context precedes the complete source, contains summary text without added node addresses, and includes the correct level-0/merge context boundary. Merge inputs flatten child newlines; free parents retain the specified joining newline. | Matches |
| §4 compression | Exact 512-byte scale example, byte-safe cut feedback, five attempts in the same conversation, trimmed outputs, shortest attempt retained, empty output rejected. Local HTTP fixtures cover retries for both providers. | Matches |
| §4.4 prompt | `compact.txt` matches the verbatim COMPACT block in SPEC.md. | Matches |
| §5 view | Incremental append and fit, real text byte sizes, largest age/weight sibling merge, only built parents, no splitting. Reload folds messages from zero with the stored tree. Existing tests cover ordered compaction and stable coarsening. | Matches by source inspection and focused tests |
| §6 settlement | Master starts only when all current view parts are built; compactor eligibility prevents unfinished context. Cancel preserves queued input in ROOT. | Matches; canceled wait tested |
| §7 fresh turns | Runtime renders the view before logging new input; subsequent turns start with a new message list. Completed reply/tool blocks and capped tool results are persisted. New input during a turn is delivered at a tool boundary. | Matches; fresh turns and tool loops tested, mid-run injection timing not independently stress-tested |
| §7 prompts and tools | `master.txt` contains verbatim MASTER and VIEW_DOC blocks, followed at runtime by user instructions. `zoom` and `date` are available. | Memory tools match; general vendor/agent tools absent |
| §8 Anthropic caching | Stable system/tools, line-boundary view marks, automatic request-end caching, no one-hour TTL or renewal pings. Signed thinking blocks are replayed unchanged. | Request layout tested; actual cache reads/costs unverified |
| §8 DeepSeek | Uses the Anthropic-compatible transport, preserves reasoning during the tool loop, omits explicit cache markers for automatic caching. | Requested provider adaptation; live behavior unverified |
| §9 subagents | No spawn/tell/computer workers. | Explicitly optional in the spec |
| §10 interface/runtime | Native desktop app while open; Memory tab instead of printing the view at terminal startup. | Requested egui adaptation; no always-on remote daemon or attachment |
| §10 browsing | HTML includes current view, ROOT, and all tree levels with escaped text, ranges, sizes, and timestamps. | Metadata gap fixed in this audit |
| §10 import | Plain UTF-8 lines become new notes. No historical IDs, dates, session adapters, or import deduplication. | Partial; chronological session importer is a separate PRD |
| §10 persistence/backup | Every record is durable before publication, stronger than only persisting at turn end. | Persistence implemented; automatic backup/Git commits absent |
| §11 prohibited approaches | No hybrid raw blocks, per-turn retiling, truncated unfinished view, context-free compactor, reasoning log, volatile system prompt, or exponential retry. | None found in the inspected paths |

## Remaining limitations

- A malformed record in the middle of a log can leave an ID gap. Loading then fails the contiguous-ID check rather than opening a partial history or renumbering permanent IDs. Likewise, a torn final message with existing tree records referencing it is rejected. The implementation safely refuses these cases but does not provide general corruption recovery.
- The master currently has only memory tools. It cannot perform filesystem, shell, web, or other agent actions described by the broader product premise. Optional subagents do not fill this gap.
- Session imports described in [the importer PRD](/Users/benj/Documents/general/docs/prds/optchat-session-memory-importer.md) remain proposed work. Existing note import must not be described as that importer.
- The prompt retains the spec's optional subagent/background wording even though those tools are unavailable. The API's actual tool list remains limited to zoom/date.
- Provider models must support the configured thinking options. No OpenAI Responses transport is implemented; its vendor-specific §8 rules are consequently not exercised.
- Exact recall quality, long-history cache reuse, actual API cache counters, remote operation, and Windows/Linux runtime behavior are not established by local tests.

## Verification

- Gist API revision/content comparison and verbatim prompt comparison.
- Existing memory, runtime, and credential suites, including local SSE tool-loop fixtures for both providers.
- Export regression: observed failure for missing current-view byte metadata, then passed with range/size/date assertions.
- Final checks: `cargo test --locked`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo build --release --locked`.

No existing chat records, credentials, importer implementation, deployment settings, or Git history were changed by this audit.
