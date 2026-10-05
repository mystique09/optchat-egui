# MCP client and local skills

Status: initial delivery implemented and locally verified. Requested October 5, 2026. Compatibility exclusions below remain follow-up work.

## Goal

OptChat connects to user-configured MCP servers and uses locally installed Agent Skills. It is an MCP client, not a new MCP server. Integration is protocol-based and supports multiple servers without server-specific adapters. Default skill discovery is `~/.agents/skills`.

## Sources and existing behavior

- [MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports): stdio and Streamable HTTP, negotiated sessions, JSON/SSE responses, cancellation.
- [Agent Skills format](https://agentskills.io/specification): directories containing SKILL.md with YAML name/description metadata and optional relative reference files.
- Existing OptChat: one durable chat, fresh turns, stable tool definitions within a turn, zoom/date, capped logged tool results, egui Settings, Tokio worker. Preserve these behaviors and both model providers.

## Product behavior

1. Settings exposes Add/Remove controls for MCP servers and skill paths, transport-specific editable fields, Save and reload, and connection/discovery status. A skill path may be a single skill folder or a directory of skills. Removing a path does not delete files. Configuration persists separately from message logs under the chat directory. No server is configured or started automatically merely because a skill mentions it.
2. Users can configure multiple named stdio or Streamable HTTP servers. Stdio accepts executable, arguments, working directory and environment-variable mappings. HTTP accepts a URL and optional bearer-token environment-variable name. Config contains references to secrets, not provider keys. Tools retain original schemas and are namespaced deterministically so duplicate names on different servers work.
3. Use the maintained official Rust SDK for initialization, version negotiation, sessions, pagination, and protocol errors. Isolate a failed server so others remain usable. Reload reconnects servers and refreshes tools between turns. Freeze the resulting tool catalog for an entire turn; do not retry failed tool calls automatically because they may have side effects.
4. `/permissions` opens a local selector for Ask for approval (default) or Full access, persisted across restarts. Ask requires Allow once or Deny for each external tool call, showing server, tool, and arguments. Full access skips prompts for all configured MCP tools and releases a pending approval. The selector is also reachable from the toolbar and approval dialog. Server annotations never grant permission. Stop cancels pending approval and in-flight requests; timeouts return errors. UI and memory compaction remain responsive during external calls.
5. Forward text and structured MCP results as capped tool-result text, preserving the error flag. Non-text content is described by type without pretending the model received media. Log calls and returned results through the existing tool/echo path. Do not log transport credentials.
6. Discover SKILL.md recursively within configured roots, including installed symlinked skill directories. Parse YAML frontmatter; list name, description, and an unambiguous ID. Report malformed or unreadable skills without blocking valid ones. Default roots contain `~/.agents/skills`; users may replace or extend them.
7. Advertise skill names/descriptions to the model through a stable discovery-tool description. `load_skill` reads the complete instructions on demand; `read_skill_file` reads a relative UTF-8 reference within that skill's resolved directory. Reject absolute paths and traversal/symlink escapes. Large files are paged explicitly rather than silently truncated. Skills never create tool permissions or execute scripts automatically. A script can be run only through an explicitly configured execution tool and its normal approval flow.
8. Reload config and skills while idle. Reject invalid config without replacing the working integration. A chat turn already in progress keeps its catalog. Defaults work even when the skills directory is absent.

## Compatibility boundary

This delivery implements MCP **tools** over stdio and Streamable HTTP, with unauthenticated, bearer-token, or browser OAuth HTTP access. OAuth uses SDK discovery and PKCE, dynamic registration or a preregistered public client ID, a loopback callback, Keychain token persistence, automatic refresh, cancellation, and local sign-out. Tokens must never enter chat logs or integration JSON. OAuth operations run outside the UI and actor loops; configuration/auth changes require an idle chat. A server-rejected token may trigger one SDK refresh and resend; other tool failures are not retried. Hosted client metadata documents, confidential client secrets, automatic scope-upgrade dialogs, deprecated standalone HTTP+SSE transport, resources/prompts UI, sampling, elicitation, roots, and asynchronous task extensions are outside this delivery.

The skill loader supplies instructions and references. Host-specific skills may require tools OptChat does not have; the model must report that limitation rather than invent execution results.

## Acceptance and verification

- Real protocol fixtures exercise two simultaneous servers with colliding tool names, initialization, pagination, exact routing/arguments, text/structured/error results, timeout/cancellation, and failure isolation.
- Exercise both stdio and Streamable HTTP through the SDK, without paid provider calls or external account mutations.
- Approval denial sends no tool request; allow executes once. Stop releases pending approval and cancels active execution. Late results cannot resume a canceled/new turn.
- Local skill fixtures cover real YAML syntax, default path expansion, nested and symlinked discovery, duplicate names, malformed frontmatter, exact loading, relative references, pagination, and path escapes. Inspect the actual installed skill catalog read-only.
- A local model fixture proves the advertised MCP/skill tools can complete the existing model loop and that tool outputs are durable while reasoning remains excluded.
- Existing tests, formatting, Clippy, release build, and native UI checks pass. Report tested protocol behavior separately from live provider/server authentication.

## Implementation shape

Keep skill filesystem discovery/loading in `skills.rs`; keep protocol connections, configuration and tool routing in `integrations.rs`. These have distinct lifecycles from the canonical memory log and do not replace it. Extend the existing runtime actor for asynchronous external tool batches and approval events rather than introducing another job queue. The UI only edits configuration and answers approvals.

## Delivery evidence

- Local protocol fixtures negotiated MCP 2025-11-25 and exercised stdio plus Streamable HTTP with JSON and SSE responses. Two servers with duplicate tool names routed correctly; paginated discovery, failure isolation, denied approvals, approved calls, tool errors, timeouts, and explicit cancellation were verified.
- Skill fixtures verified multiline YAML, symlink discovery/deduplication, duplicate names, malformed files, exact text, Unicode pagination, missing roots, and rejection of relative/absolute/symlink escapes. The real default directory yielded 120 skills and zero warnings; a skill was loaded read-only.
- A model fixture loaded a skill and invoked an MCP tool through the normal turn loop. Assertions covered durable echo records, stable tool definitions, mid-run input, invalid-config preservation, cancellation before execution, and a fresh subsequent turn.
- Native macOS QA used an isolated chat and local fixture model/server: Settings displayed 120 skills and two server tools; Allow once showed the exact arguments, executed the call, and returned a completed response to chat. No external account or paid model call was used.
- Tests require Python 3 for protocol fixtures. OAuth fixtures verify discovery, PKCE, callback state rejection, token exchange, expired-token refresh, restored credentials, authenticated MCP discovery, resource isolation, sign-out, and cancellation. Live provider consent and non-macOS runtime behavior require separate verification.
