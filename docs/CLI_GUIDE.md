# Builder CLI and configuration reference

[Back to the quick start](../README.md) · [Remote control](REMOTE_CONTROL.md)

## Work from your terminal



The UI uses a multiline composer, lavender accents, a live context/status line, command suggestions, streaming text, and elapsed-time progress. It keeps ordinary terminal scrollback and text selection. `NO_COLOR` disables color; `--plain` gives you a basic prompt for limited terminals.

### Fast input, including large pastes

On macOS, **Ctrl+V reads the clipboard directly** using `pbpaste`, bypassing slow terminal clipboard delivery. Use it instead of Cmd+V if large pastes stall. It inserts text into the draft without submitting. Clipboard reads time out after two seconds and oversized text is rejected in full. Cmd+V still uses the terminal’s normal paste path.

Bracketed paste is handled as one insertion. Pastes of 256 bytes or more, or at least three lines, become compact blocks such as `[paste 1 · 121 lines · 5.6 KiB]`. Their full contents are retained, including indentation and line endings, and are expanded only when you send. Paste never sends by itself. You can type before/after a block, remove it with Backspace, and restore it with Undo. Blocks are atomic; their contents are not edited inline.

The composer paints only a bounded viewport and batches bursts of key events. Large blocks use shared immutable storage so typing, layout, undo, and history navigation don't copy the pasted payload. Drafts are capped at 4 MiB; an oversized insertion is rejected in full. History and undo also have memory/count budgets. After submission, the UI distinguishes the saved prompt awaiting the endpoint, an established connection, model reasoning, and tool-call preparation. Reasoning activity is indicated without adding intermediate reasoning to conversation history. While the model responds, terminal output is coalesced on a 16 ms timer or at an 8 KiB threshold, then flushed at completion and tool/retry boundaries.

| Key | Action |
| --- | --- |
| Enter | Send the complete draft |
| Alt+Enter / Ctrl+J | Insert a newline (Shift+Enter also works when the terminal reports it) |
| Left / Right, Home / End | Navigate text and paste blocks |
| Up / Down | Navigate visible lines; access history at the first/last line |
| Ctrl+P / Ctrl+N | Previous/next history entry; returns to your unsent draft |
| Ctrl+V (macOS) | Paste directly from clipboard |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| Ctrl+W / Ctrl+U | Delete previous word / clear draft |
| Tab / Shift+Tab | Complete / select suggested slash commands |
| Ctrl+C | Clear a draft; exit if empty; cancel a running generation/tool |
| Ctrl+D | Exit an empty composer |

Input history lasts for the current process. Submitted conversations remain durable in SQLite. The composer is active between generations; this version does not provide background draft editing while a model is responding. See [terminal performance and test methodology](TERMINAL.md).

| Command | Purpose |
| --- | --- |
| `builder` | Interactive conversation |
| `builder --auto` | Automatically approve all tool actions, including edits and shell commands |
| `builder -C /path/to/repo` | Work in a specific directory |
| `builder run "Explain this repository"` | One task; final answer on stdout, status on stderr |
| `cat task.txt \| builder run -` | Read a task from stdin |
| `builder sessions` | List saved sessions |
| `builder resume` | Resume the most recently updated session |
| `builder resume 5e20a117` | Resume by unique ID prefix |
| `builder run --session 5e20a117 "Now add tests"` | Continue a session in a script |
| `builder export 5e20a117 --json` | Export full messages and tool calls |
| `builder models` | Discover endpoint models |
| `builder doctor` | Validate config/storage/discovery and actively probe generation, JSON, and native/parallel tool calls |
| `builder config show` | Inspect all profiles |

Auto-compaction is enabled by default. At 75% of the configured window (estimated input plus tool schemas and reserved output), Builder summarizes older context and continues. `/compact` runs it manually. Checkpoints preserve the exact system instructions, latest user request, recent complete tool exchanges, and explicit denial/uncertainty constraints. A deterministic inventory of the newest 32 distinct returned file ranges (up to 4 KiB of entries) is rebuilt from original results at each compaction, including legacy sessions. It survives a model omitting those filenames; it records historical reads, not proof of understanding or current file contents. The summary prompt retains per-file findings, unresolved questions, and the next concrete action. Earlier context becomes a **lossy handoff summary**; original messages remain on disk in `/history` and `/history archived`. Summaries are saved only after all chunks succeed; cancellation or failure preserves the previous context. Oversized restored sessions are summarized in bounded fragments. An oversized latest prompt or system instruction is never silently shortened.

Configure `auto_compact = false` to disable automatic summaries, or `compact_at_percent = 75` (50–90) to change the threshold in a model profile. Status shows the setting. These defaults also apply to existing profiles missing the new fields. Compaction uses the same endpoint without tools and adds model requests; it does not make inference faster. Its generation budget follows `max_output_tokens` up to 16,384. The accepted handoff allowance scales with the material being replaced and the context window, from 4,096 to 16,384 estimated tokens; it can use at most one quarter of the older context and one tenth of the full window. Hidden reasoning is stored in the transcript but excluded from prompt estimates and handoff sizing because providers do not send it back. If a fragment hits the output limit, Builder discards that partial summary and retries it once with twice the allowance (up to 32,768), reserving room in the context beforehand. A genuinely oversized visible handoff gets one separate shortening retry from the original evidence. Empty, tool-bearing, and oversized results report distinct errors; none replace saved history.

Output limits are separate from context limits. On a `length` finish, Builder discards the incomplete result and retries once with up to twice the output budget (capped at 32,768 and available context). It never executes partial tool calls. If the larger response also hits its limit, the turn remains saved for a new instruction or `/retry`.

Inside a conversation: `/help`, `/status`, `/history`, `/history archived`, `/retry`, `/compact`, `/cancel`, `/rewind`, `/attach PATH`, `/exit`.

During a response, **Ctrl+C pauses the agent and returns to the composer**. Send a new message to add context or change direction; completed messages and tool results remain in context, incomplete model output is discarded, and queued tools from the interrupted turn are cancelled. `/retry` instead continues from the saved context. Opening an unfinished session with `builder resume` now waits for your instruction rather than restarting the model automatically.

Use `/cancel` after pausing to end the pending response without sending another request. Use `/rewind` to archive the latest user message and everything after it, then restore that message as an editable draft. Edit it and press Enter to restart from that point; repeat `/rewind` to move back another user turn. The restored original draft survives restarting Builder. Prompts over 16 KiB are folded as paste blocks; small prompts remain directly editable. In `--plain` mode, Enter resends the displayed draft and typing replaces it.

Rewind changes conversation context only: **file edits and other tool side effects remain**. The original transcript is retained in `/history archived`, and old tool IDs remain reserved. An interrupted tool with an uncertain outcome still stops for inspection—even with `--auto`. When a follow-up discovers that condition, the new message is saved; inspect the workspace and use `/retry` to continue.

`/attach` sends small files (up to 200 lines and 12 KiB of formatted output) through the workspace read tool into the conversation. For larger files, send the path and describe the relevant question so the agent can search and read a targeted range. The workspace's root `AGENTS.md` is captured in the system message when the session is created. The model is instructed to respect additional applicable instructions it discovers.

## Persistence and recovery

- **Write before send.** The user message is durable before any model request starts. Every request contains the complete committed conversation and tool results.
- **Restartable generations.** Connection errors, timeouts, HTTP 408/429, and selected 5xx errors retry with bounded exponential backoff and jitter. Numeric `Retry-After` is supported, capped at five minutes. Authentication and invalid requests fail immediately.
- **Partial output is provisional.** It can appear in the interactive terminal, but isn't committed and cannot trigger tools. On retry, the UI marks it discarded. Headless stdout contains only the final committed answer.
- **Crash recovery.** SQLite uses WAL, `synchronous=FULL`, transactional writes, and a versioned schema. An exclusive session lock prevents two processes from driving the same session.
- **No blind tool replay.** Tool calls are durable before execution. Results and tool messages commit in one transaction. An interrupted tool with no durable result is marked uncertain and recovery stops for inspection.
- **Bounded work.** Connection, idle, and total request timeouts; response size limits; bounded tool output; a maximum number of agent rounds; and explicit context budgeting.
- **Explicit compaction with originals retained.** By default, Builder summarizes older context at 75% of the configured window. Checkpoints are labelled and original transcript rows remain available. Disable automatic summaries with `auto_compact = false`; uncompactable input stops without dropping history.

Most compatible endpoints cannot continue a disconnected generation at a specific token. Builder resends the same committed input; a fresh generation may produce different text or consume additional server compute. It does **not** promise exactly-once remote inference or transactional execution of arbitrary shell commands.

## Workspace tools and permissions

The agent can `list_files`, `read_file`, `search`, `write_file`, `edit_file`, and `shell`.

`read_file` allows whole-file reads up to 200 lines; larger files require a line range. Each read is limited to 500 lines and 12 KiB of formatted output. A lone `start_line` reads the next chunk forward, a lone `end_line` reads the chunk ending there, and ranges clamp to the file with explicit notes about preceding or remaining lines, so chunked reads can continue without guessing. A range that overflows the byte budget returns no source but names the exact line where the budget ran out and the range to retry. Search returns at most 30 matches and 8 KiB, with explicit truncation notices. Targeted reads always read current file contents; previously inspected files are not blocked or served from a stale cache.

| Mode | Reads | File edits and shell |
| --- | --- | --- |
| `--approval ask` (default) | Automatic | Each action asks; denied if stdin isn't a terminal |
| `--approval read-only` | Automatic | Denied |
| `--auto` or `--approval trust` | Automatic | Authorized automatically |

`--auto` works with chat, `run`, and `resume`. It is an invocation flag, is not saved as a default, and cannot be combined with an explicit `--approval`. The banner/status show auto mode. It skips approval prompts; workspace boundaries, timeouts, and uncertain-execution safeguards still apply.

The approval prompt shows the full proposed content, exact replacement, or shell command. Edits require an unambiguous old-text match. Replacements use a temporary file plus rename; existing files are backed up under `.builder/backups/`. Parent directories must already exist. Search and listing respect ignore files. File tools reject workspace escape paths and symlinks pointing outside the workspace, and protect `.git` and `.builder` internals.

Shell commands run with **your user permissions**, not in an OS sandbox, and can access paths outside the workspace. On Unix, Builder owns a process group and terminates its non-detached descendants when a tool completes, times out, or is cancelled. Deliberately detached descendants are outside that guarantee; Windows currently kills the direct child only. File path checks are not an OS security boundary against concurrent malicious filesystem changes.

## Configuration

Edit `~/.config/builder/config.toml` directly. `$XDG_CONFIG_HOME/builder/config.toml` overrides that default config root. On Windows the default is `%USERPROFILE%\.config\builder\config.toml`. `builder config init` creates a default file if absent and prints its path; the other config commands are optional editing helpers.

The SQLite database, WAL files, session locks, model downloads, and remote token remain in the platform's local application-data directory. `BUILDER_HOME` or `--home` explicitly keeps both config and data in the chosen directory. On first default launch, Builder copies a legacy data-directory config to the normal config directory if it has no config yet. The copy preserves comments and credentials, uses owner-only permissions on Unix, and never overwrites an existing destination. Original config and data remain in place; subsequent edits belong in the new file. Config files are bounded to 1 MiB.

Conversations and tool output are stored in plaintext locally; endpoint keys are kept out of the database by Builder's transport.

```toml
default_profile = "local"

[profiles.local]
base_url = "http://localhost:11434/v1"
model = "qwen3:8b"
# api_key_env = "MY_MODEL_API_KEY"
stream = true
tools = true
context_tokens = 32768
max_output_tokens = 4096
max_attempts = 5
connect_timeout_secs = 10
idle_timeout_secs = 90
request_timeout_secs = 900
```

### Import Continue or OpenCode models

```sh
builder config import ~/.continue/config.yaml --format continue --activate
builder models
# Correct an outdated server model ID without resetting auth or sampling settings:
builder config model 'the-exact-id-returned-by-builder-models'
builder doctor
builder

# OpenCode custom providers, including JSONC comments and trailing commas:
builder config import ~/.config/opencode/opencode.json --format opencode --activate
```

Imports copy model settings into Builder's `config.toml`; they do not modify or continuously watch the source file. Existing profile names require `--replace`. `--activate` selects the imported chat model as the default. Continue model names become profile names; OpenCode models become `provider-id/model-id`. Use `--profile 'profile name'` to select another model.

The supported [Continue v1 model fields](https://docs.continue.dev/reference) include `provider: openai`, `apiBase`, `apiKey`, roles, tool capabilities, completion settings, and request headers/timeouts/extra body properties. This covers custom servers exposing Chat Completions. Sampling maps to `temperature`, `top_p`, `top_k`, `min_p`, `presence_penalty`, `frequency_penalty`, and `stop` on the wire. Imported millisecond request timeouts round up to seconds and apply to both total and idle timeouts, preserving long prompt-prefill waits; native config can tune those independently.

The [OpenCode custom-provider subset](https://opencode.ai/providers/) supports `npm: @ai-sdk/openai-compatible`, provider `options.baseURL`, `apiKey`, `headers`, `timeout`, `maxRetries`, provider-level `temperature`/`topP`/`topK`, model IDs, limits, headers, and extra body options. Native provider protocols, OpenCode's auth database/OAuth, per-model provider overrides, and Continue proxy/custom-CA/TLS settings are not imported. Unsupported request settings fail; unrelated application settings produce warnings. These are model import adapters, not full replacements for either application's configuration system.

Embedding profiles retain their endpoint and auth, and can use `models`/`doctor`. Builder rejects them for chat. Local CPU embeddings and explicitly configured remote embeddings support persistent hybrid semantic/keyword retrieval. See [memory](MEMORY.md).

### Authentication and advanced settings

See the [README Basic Auth walkthrough](../README.md#custom-endpoint-with-http-basic-auth) for a complete config and credential prompt command.

API keys and every custom header accept a literal string or an environment reference in native TOML:

```toml
[profiles.hosted]
base_url = "https://models.example.com/v1"
model = "your-model-id"
api_key = { env = "MODEL_API_KEY" }
context_tokens = 81920
max_output_tokens = 8192
request_timeout_secs = 1800
idle_timeout_secs = 1800

[profiles.hosted.headers]
# Variable contains the entire header value, e.g. Basic <base64(user:password)>.
Authorization = { env = "PANGOLIN_AUTHORIZATION" }
X-API-Key = { env = "GATEWAY_KEY" }

[profiles.hosted.completion]
temperature = 0.6
top_p = 0.85
top_k = 20
min_p = 0.0
presence_penalty = 0.0
frequency_penalty = 0.0

[profiles.hosted.extra_body.chat_template_kwargs]
enable_thinking = false
```

**An explicit `Authorization` header wins over the API key**, case-insensitively. Builder sends exactly one authorization value on discovery and every generation attempt. Without that header, `api_key` or the legacy `api_key_env` becomes bearer auth. Redirects remain disabled. Headers cannot override HTTP routing or message framing, and extra body properties cannot replace messages, model, tools, stream mode, or output limits.

Imports recognize whole-value `{env:NAME}` and `${{ secrets.NAME }}` references as local environment variables. Continue's remote secret service is not contacted. Embedded templates and `{file:...}` references are rejected; use a literal or whole-value environment reference instead.

Literal credentials remain plaintext in the local config, saved atomically with mode `0600` on Unix. Windows uses the user's directory ACLs. `config show` and credential `Debug` output redact values; parser errors suppress source excerpts. Do not put credentials in `extra_body`, model names, or URLs. Environment references avoid copying credential values into Builder's config. Neither config auth nor resolved credentials are added to conversation history by the transport.

Builder uses each selected profile's `context_tokens` value directly. Automatic summarization occurs only when the estimated request reaches `compact_at_percent` of that configured window and `auto_compact` is enabled; `/compact` is the only other path that creates a summary checkpoint. The progress and failure guards never summarize or archive active context.

Builder checks for stalled work using the original unrewound history, including calls archived by normal context compaction. By default, 12 completed tool calls without a successful file edit add focused recovery guidance and temporarily remove broad list/search discovery; the full active conversation remains intact. Successful shell output, memory updates, failed or denied mutations, and model-authored status do not masquerade as file progress. A successful typed file edit or new user instruction resets the counter.

The guard settings are saved per profile and editable in `/settings`: `progress_check_calls`, `progress_recovery_rounds`, `failure_check_calls`, `failure_recovery_rounds`, `identical_shell_calls`, and `tool_calls_per_response`. Defaults permit 100 no-progress calls (12 before focused recovery plus 88 recovery rounds), three identical shell calls, and 128 calls in one response. Oversized, malformed, duplicate-ID, reused-ID, and unknown-tool batches are rejected before execution. Saved pending batches are checked before every unclaimed call, while interrupted claimed calls retain uncertainty precedence and are never replayed. An identical execute or mutation request following a denied or uncertain outcome remains blocked until the user sends a new instruction. These are model-independent liveness guards, not a guarantee of task completion.

An opt-in live test exercises a resumed compacted investigation against the configured endpoint in a disposable fixture, allowing only edits to its fixture file (no shell):

```sh
BUILDER_LIVE_CONFIG_HOME="/path/to/builder/config-directory" cargo test --test live_progress compacted_investigation_reaches_a_real_edit -- --ignored --nocapture
```

The context estimate uses serialized bytes divided by two plus message overhead; tool definitions and reserved output also count. It is a planning estimate, not the model's tokenizer. Match `context_tokens` to the actual server context setting. The compaction status line displays both the estimate and configured window so the active value is visible. A server can still reject a request if its tokenizer or configured limit differs.

Resume uses the saved workspace and profile name. Profiles are resolved at startup, so editing a profile's endpoint/model changes subsequent runs. `--profile` can explicitly override the saved profile. No API keys are copied to the session. `doctor` runs active protocol conformance probes and stores their structured results by a non-secret endpoint/model/settings fingerprint. A passing probe establishes the tested protocol behavior, not coding quality.

## Architecture and development

Four crates, a one-way dependency graph, and no application `unsafe` code:

```text
builder (CLI + application engine)
  ├── builder-provider (Provider trait, HTTP retries, SSE decoding)
  │     └── builder-core
  ├── builder-tools (typed actions, workspace I/O, subprocess lifetime)
  │     └── builder-core
  └── builder-core (typed messages, config, durable session repository)
```

Read [ARCHITECTURE.md](../ARCHITECTURE.md) for the state machine, invariants, tradeoffs, and extension seams.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
python3 tests/terminal_smoke.py target/release/builder
```

Real-model behavior is tested separately with objective file and answer graders. See [LLM_EVALUATIONS.md](LLM_EVALUATIONS.md) for live runs, repeated trials, reports, and limitations. These opt-in tests use disposable synthetic workspaces and deny shell execution.

Tests inject incomplete streams, service errors, rate limits, fragmented UTF-8/tool arguments, and interrupted executions. They also exercise storage reopening, exclusive locks, context preservation, exact edits, ignore rules, path boundaries, and subprocess limits. CI runs on Linux, macOS, and Windows.

## Planned features

The current adapter implements Chat Completions text/tool messages. Native provider protocols, multimodal input, model-specific tokenizers, MCP, an OS sandbox, workspace indexing, and a full-screen TUI remain future work. These additions must preserve the existing persistence guarantees and include failure tests.

Protocol references: [Ollama's compatible API](https://docs.ollama.com/api/openai-compatibility) and [reqwest timeout controls](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html).

MIT licensed.

## Persistent memory

Builder can retain short, source-backed findings across compaction and sessions, automatically retrieve them, and refresh stale findings through revisioned updates. Open `/memory` and choose local embeddings, or run `builder memory enable --local`. A one-time model download enables on-device semantic/keyword retrieval; vectors remain in SQLite. Remote embeddings require explicit opt-in. Global user preferences use explicit `builder memory remember KEY TEXT`. Memory is reference data, never additional authority, and source-version agreement does not prove a model's interpretation is correct. See [configuration, commands, retention, and limits](MEMORY.md).

## Evidence-driven engineering

Builder's `research` tool adds acceptance criteria, falsifiable hypotheses,
versioned contract observations, optional language-server navigation, isolated
candidate experiments, independent test-coverage review, and phase-aware learned
procedures. Changed sources invalidate affected evidence; stale procedures are
withheld until revalidated. A recorded plan must finish as verified, unverified,
or blocked before the runtime accepts completion.

These tools use the current model and existing approval/recovery rules. Candidate
copies are not OS sandboxes. See [the research runtime guide](RESEARCH_RUNTIME.md)
for source-scope limits, examples, and the same-model evaluation harness.

In interactive chat, press `/` and choose **Settings** to toggle features and edit
limits in a menu. **Save settings** applies changes immediately and saves them to
the current profile; **Cancel** keeps the previous settings.

For scripting, configure features per endpoint profile or override them for one invocation:

```sh
builder config pipeline show
builder config pipeline set candidates=false review=false
builder config pipeline set candidate_attempts=5 analysis_timeout_secs=30
builder --pipeline enabled=false run "Explain the code"
builder config pipeline reset
```

Use `--profile NAME` to select the saved profile. Repeated `--pipeline KEY=VALUE`
overrides also work with chat and resume and are never persisted. The
[configuration reference](RESEARCH_RUNTIME.md#cli-pipeline-configuration)
lists every feature switch, budget, default and dependency.

Run budgets default to 100 model rounds. Open `/settings` and edit **Agent rounds per run** (1–1000) to save a different budget for the profile and apply it immediately. `/status` shows the active budget. `/retry` continues saved pending work with a fresh run budget; loop guards still apply. An optional `--max-rounds` overrides the saved value at startup.
