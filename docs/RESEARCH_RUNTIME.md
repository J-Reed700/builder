# Evidence-driven engineering

Builder exposes the `research` tool when the profile supports tools. It runs beside the existing vector/lexical memory, without replacing that agent's extraction or storage. No model retraining, additional model account, or automatic language-server installation is required. The current profile handles independent analysis and review calls.

The runtime supplies workflow guidance on every request, including resumed sessions. For implementation it asks the model to record acceptance criteria, gather current evidence, form a falsifiable hypothesis, run checks, get an independent coverage review, and record an honest finish. The model still chooses its actions. Planning is not forced on ordinary conversation; once a plan exists, the runtime enforces the completion gate.

## Operations

All operations use `research({"request":{"operation":"...", ...}})`.

| Operation | Purpose / required fields |
| --- | --- |
| `plan` | `criteria`: 1–8 acceptance criteria. Fixed for this user turn; replacing the request requires a new user instruction. |
| `observe` | `artifacts`: current source versions with file, JSON-pointer, or exact unique text selectors. |
| `symbols` | `query`, optional `glob`: Rust AST declarations/path references, explicitly labelled lexical candidates for other languages. It does not pretend to resolve types. |
| `semantic` | `server`, `args`, `path`, `line`, `character`, `feature`: an installed stdio LSP server for definitions, references, or diagnostics. Positions are zero-based UTF-16. |
| `hypothesis` | `claim`, `falsification`, `evidence_ids`: a proposed explanation supported by fresh `observe` calls. It remains a hypothesis after a passing check. |
| `verify` | `criterion`, `command`, `dependencies`; optional `hypothesis_id`, `timeout_secs`: measure a real-workspace command and capture source versions before/after. Match a planned criterion exactly. |
| `candidate_test` | `hypothesis_id`, `criterion`, `patches`, `command`; optional `timeout_secs`: test an alternative in a disposable source copy. |
| `candidate_apply` | `candidate_id`: apply a passing alternative only if the original source snapshot and exact patch preconditions still match. |
| `review` | `verification_ids`: a separate, tool-free model call critiques check coverage against the user request and criteria. Vacuous checks should receive `needs_work`. |
| `finish` | `outcome`: `verified`, `unverified`, or `blocked`; `explanation`: what was established and what remains uncertain. |
| `learn` | `key`, `phase`, `procedure`, `applicability`, `verification_ids`; optional `supersedes`: provisional reusable procedure supported by current checks. |
| `recall` | `query`, `phase`: retrieve matching current procedures or stale leads. Phases: locate, diagnose, implement, verify. |
| `retire` | `key`, `reason`: withdraw a procedure while retaining its original journal records. |
| `history_search` | `query`, `include_archived`; optional `before_seq`: search original session messages using keyset pagination. |
| `history_read` | `seq`, `offset`: page the exact original message JSON without silently removing context. |
| `analyze` | `question`, `artifacts`: independent analyses of up to four bounded source excerpts and one synthesis. |
| `status` | Current acceptance criteria, latest checks, coverage review freshness, and recorded outcome. |

Artifact examples:

```json
{"path":"src/api.rs","selector":{"kind":"file"}}
{"path":"contracts.json","selector":{"kind":"json_pointer","pointer":"/responses/order"}}
{"path":"src/api.rs","selector":{"kind":"anchor","text":"pub fn lookup(id: Id) -> Option<Order>"}}
```

An exact anchor must occur once. A changed or ambiguous anchor fails and must be relocated. JSON selectors hash the selected parsed value, so unrelated key changes do not invalidate that observation. Observation excerpts are previews; hashes cover the entire selected value. Use normal bounded reads for complete source.

For patches provide `path`, `source_hash` from a current observation/read, `old` and `new`. Each patch must replace one unique exact block. One patch per physical file is allowed; creating entirely new files remains a normal approved write. Applying multiple files is not a filesystem transaction: if interrupted, the existing uncertain-tool recovery rule requires inspection and forbids automatic replay.

## Freshness and contracts

Two scopes serve different purposes:

* Observations track the selected file, JSON value, or unique source anchor. Their freshness means those bytes still match, not that the interpretation is true.
* Verification and candidate results also track a broad source manifest. It includes nonignored files, including hidden configuration, and content plus permissions. Added, removed, or changed in-scope files invalidate a check. This deliberately favors false invalidation over trusting an incomplete dependency graph.

The manifest excludes `.git`, `.builder`, `target`, `node_modules`, and `__pycache__`, and respects ignore rules. It cannot establish freshness of external services, environment variables, ignored dependencies, or tools installed outside the workspace. Explicit artifact dependencies can cover ignored files inside the workspace; checks must exercise external contracts. A later failed or denied run of the same check command supersedes an older pass even without source changes.

A check that modifies in-scope sources is not considered a stable passing verification, even when it exits zero. Run generators/formatters first and then verify again. Missing/deleted dependencies, unavailable snapshots, rewound evidence and missing proof records fail closed. There is no timestamp grace period and no persistent hash cache that can conceal external edits. A concurrent change that occurs and is restored entirely between snapshots is outside this before/after mechanism's guarantees.

Procedural retrieval checks proof freshness on every use. A stale procedure returns only its key/record ID and a revalidation instruction; its procedure text is withheld. To update it, inspect the changed contracts, run the appropriate checks, and save a new `learn` record with `supersedes`. Retrieval is phase-specific and is also added automatically to model requests. Retired keys stay withdrawn while their records are in the bounded retrieval window; use a new key for an explicitly new procedure. These are model-derived procedures supported by checks, not universally validated rules or measured improvements in future task success.

## Completion, recovery and permissions

A verified finish requires a current passing check for every planned criterion and a fresh `adequate` independent review covering those checks. The reviewer is another bounded call to the same model, not a distinct trusted authority. A user follow-up establishes a new turn and new criteria; a rewind removes the old evidence from active retrieval.

After a plan has been recorded, an ordinary final answer without a valid finish is rejected. Its complete text is retained in the attempt audit, the model receives the durable status again, and at most two corrective final attempts are allowed per run before pausing with the conversation still pending. This cannot guarantee that the model never utters an incorrect claim; streaming text is provisional until the runtime accepts it. An honest unverified/blocked finish is always available. Plain conversations without a recorded plan retain their existing behavior.

Research records are views over original assistant calls and atomically completed tool results. There is no second mutable database state machine or new schema migration. Results printed by unrelated shell commands cannot masquerade as research proof. Scope is the canonical workspace path. Compound evidence IDs are `session_id:call_id`; bare call IDs refer only to the current session.

The existing agent journals the whole call batch, claims each call, then checks permission before dispatch. Verification, candidate commands and language-server launches require Execute permission. Candidate application and retirement require Write permission. Read-only mode denies those operations. Claims without results are never replayed automatically, including after cancellation of an analysis call. Temporary candidate directories and child processes have owned lifetimes and are cleaned up on completion/cancellation.

A candidate copy is **not an OS sandbox**. Its command still runs with the user's permissions and can address paths or services outside that copy. Starting an LSP server can also execute project tooling. The client rejects server-requested workspace edits, bounds framing, suppresses results when source versions change, and never interprets absent diagnostics as a clean bill of health. Semantic results may be incomplete while a server indexes. See the [LSP specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/).

## Bounds

* Source snapshot: 20,000 files, 2 MiB/file, 64 MiB total, no symlinks. Exceeding a limit fails; a partial manifest is never proof.
* Retrieval: newest 256 committed research results; at most four procedures. Proof falling outside that window requires revalidation. Original records remain accessible through history paging.
* Candidates: at most three claimed attempts per original active user turn, including failures and interruptions. Compaction/restart does not reset the count.
* Commands: 1–120 seconds; existing subprocess output caps apply. Checks keep bounded previews and explicit measured status.
* LSP: 20-second process lifetime, 5-second diagnostic wait, 1 MiB/message, 128 messages per response, 20 KiB returned result.
* Analysis: up to four excerpts plus one synthesis; 45 seconds/call and 2,048 output tokens/call. No subcall receives executable tools. This is explicit bounded map/reduce, not unrestricted recursive agent execution.
* History: 16 search results/page; original JSON pages of up to 8,000 bytes with UTF-8-safe offsets.

## Evaluation

`tests/research_runtime.rs` covers stale/removed contracts, caller changes, changed candidates, contradictory later failures, false completion, independent review, denial, uncertain execution, persistent candidate budgets, source navigation and exact archived-history pagination. `tests/research_evals.rs` verifies that held-out graders reject the original bugs and accept known fixes.

The opt-in model evaluation compares baseline, memory, and memory-plus-research with the same profile. Every case/ablation receives a fresh workspace and store. Hidden graders run separately and are never exposed to the actor. Reports include held-out success, false verified outcomes, runtime completion, calls, tool usage, time, and serialized prompt bytes. Byte counts are not token usage; dollars are unavailable without endpoint usage/pricing. Two small Rust contract cases do not establish performance on real-world SWE benchmarks.

```sh
BUILDER_RESEARCH_EVAL_HOME="$HOME/.config/builder" \
BUILDER_RESEARCH_EVAL_PROFILE=local \
BUILDER_RESEARCH_EVAL_REPETITIONS=3 \
BUILDER_RESEARCH_EVAL_OUTPUT=/tmp/builder-research-eval.json \
cargo test --test research_evals same_model_baseline_memory_and_research_ablation -- --ignored --nocapture
```

Set the home to the directory containing your actual `config.toml`. This opt-in command uses the configured endpoint and may incur charges. It permits only fixture edits and the exact check `cargo test --offline --quiet`; language servers and arbitrary commands are denied. The benchmark harness is implemented; live model results must be collected before making claims of intelligence or cost improvements.

## Research basis

These are engineering adaptations of the research, not claims to reproduce reported scores:

* [Harness-of-Harness](https://arxiv.org/abs/2609.01481): acceptance criteria, independent checking, bounded corrective cycles.
* [SWE-Replay](https://arxiv.org/abs/2601.22129): reuse evidence and compare alternatives. Builder uses new journaled experiments, never replay of uncertain effects.
* [The Devil Is in the Interface](https://arxiv.org/abs/2608.11386): structured source/compiler tools with measured execution.
* [EA-Graph](https://arxiv.org/abs/2608.04278): artifact-bound evidence and explicit freshness. Builder uses exact selectors plus conservative manifest invalidation rather than claiming a complete inferred dependency graph.
* [Structurally Aligned Subtask-Level Memory](https://arxiv.org/abs/2602.21611), [Break It Down, Pass It On](https://arxiv.org/abs/2608.20274), and [Recuris](https://arxiv.org/abs/2608.24876): phase-aware procedural retrieval, evidence-supported updates and retirement.
* [Recursive Language Models](https://arxiv.org/abs/2512.24601) and [uncertainty-aware program search](https://arxiv.org/abs/2603.15653): explicit bounded context selection, independent analyses, synthesis and uncertainty preservation.

Disabling memory also disables automatic procedural retrieval and the `learn`/
`recall` operations. Evidence journaling, verification and explicit retirement
remain available. Acceptance plans have a separate current-turn lookup, so filling
the 256-record working window cannot silently disable the completion gate.

## CLI pipeline configuration

All research features are configurable per endpoint profile. Existing profiles
that omit `[profiles.NAME.pipeline]` keep the defaults described above. Select a
profile with `--profile`; without it, configuration commands target the default
profile. A resumed session uses its saved profile unless explicitly overridden.

```sh
# Inspect settings and currently available operations.
builder config pipeline show
builder --profile local config pipeline show

# Persist a batch of changes atomically.
builder --profile local config pipeline set candidates=false analysis=false
builder config pipeline set candidate_attempts=5 completion_retries=1
builder config pipeline set review=false semantic_timeout_secs=10

# Disable the whole added research pipeline for this profile.
builder config pipeline set enabled=false

# Restore all pipeline defaults for the selected profile.
builder config pipeline reset

# Temporary overrides: apply to this process only, including resumed sessions.
builder --pipeline enabled=false run "Explain this function"
builder --pipeline review=false --pipeline analysis_artifacts=2 chat
builder --pipeline candidates=false resume SESSION_ID

# Preview overrides without saving or calling the model.
builder --pipeline enabled=false config pipeline show
```

`set` accepts one or more `KEY=VALUE` arguments. Boolean values are `true`/`false`;
numeric values are integers. CLI keys accept either hyphens or underscores. Unknown
keys, wrong types, or out-of-range values reject the whole update without changing
the file. Transient `--pipeline KEY=VALUE` options can be repeated; the last value
for a key wins. They override saved settings without being written to disk.
Do not combine transient overrides with `set` or `reset`.

| Boolean setting | Controls |
| --- | --- |
| `enabled` | Master switch: removes the research tool, reference packet and completion gate. |
| `guidance` | Automatic workflow suggestions; does not disable configured enforcement or evidence reporting. |
| `planning` | Recording acceptance plans and activation of the completion gate. Existing plans remain in the journal. |
| `observations` | Explicit artifact/contract observation tool. Verification still captures the evidence required for its checks. |
| `symbols` | AST and lexical source navigation. |
| `semantic` | Language-server launches and semantic navigation/diagnostics. |
| `hypotheses` | Recording falsifiable hypotheses from observations. |
| `verification` | Real-workspace verification. Also makes review/learning unavailable and prevents verified finishes. |
| `candidates` | Both isolated candidate testing and application. Candidate tests still require a valid hypothesis ID. |
| `review` | Independent review operation and the review requirement for a verified finish. Current checks remain mandatory when it is off. |
| `completion_gate` | Rejection/retry of unsupported final answers after a recorded plan. |
| `procedures` | Learning, explicit recall and retirement. Learning requires verification; learning/recall also require `builder memory enable`. |
| `auto_recall` | Automatic phase-aware procedure injection; explicit recall remains available. |
| `history` | Research history search and exact page retrieval. Original messages remain stored. |
| `analysis` | Source map/reduce analysis. Independent review has its own switch. |

All switches default to `true`. Normal profile `tools=false` prevents advertising
all tools, and the separate memory setting can disable procedural retrieval even
when `procedures=true`. Turning off an operation rejects queued calls before their
implementation runs, including in trust mode. Base file tools and shell retain
their existing permissions; these switches do not create an OS sandbox.

| Budget | Default | Allowed values |
| --- | ---: | ---: |
| `candidate_attempts` | 3 | 1–20 claimed attempts per active user turn |
| `completion_retries` | 2 | 0–8 corrective final attempts per run |
| `analysis_artifacts` | 4 | 1–4 source excerpts, plus at most one synthesis |
| `analysis_timeout_secs` | 45 | 1–120 per analysis or review call |
| `analysis_output_tokens` | 2048 | 256–8192 per analysis or review call |
| `command_timeout_secs` | 120 | 1–120 maximum for research checks/candidates |
| `semantic_timeout_secs` | 20 | 1–120 total language-server session |
| `diagnostic_wait_secs` | 5 | 1–120, bounded by remaining server lifetime |
| `snapshot_max_files` | 20000 | 1–20000 |
| `snapshot_max_file_bytes` | 2097152 | 1–2097152 |
| `snapshot_max_bytes` | 67108864 | 1–67108864 total source bytes |
| `history_page_bytes` | 8000 | 256–8000 UTF-8-safe original JSON bytes |
| `history_search_results` | 16 | 1–16 results per page |
| `procedure_results` | 4 | 1–4 retrieved procedures |
| `retrieval_records` | 256 | 1–256 recent committed research records |

A requested command timeout is capped by `command_timeout_secs`; its existing
30-second default applies when no timeout is requested. Analysis requests must
also fit the model's configured context window. Reducing retrieval or snapshot
budgets never makes incomplete evidence trustworthy: unavailable proof fails
closed, and the separate durable acceptance-plan lookup remains active. Bounds
on protocol framing, patches and output remain hard safety ceilings.

Freshness checks, exact patch preconditions, approval, original-history retention
and uncertain-execution recovery are not disableable through pipeline settings.
Disabling the completion gate allows an ordinary final response, but it does not
turn stale checks into a verified finish. CLI settings never delete existing
plans, checks, learned procedures or conversation history.

## Interactive settings menu

In chat, press `/`, choose **Settings** (`/settings`) with the arrow keys, and press
Enter to select and open it. No configuration command syntax is needed inside the
panel. Use Up/Down to browse, Enter or Space to toggle a feature, and Enter on a
number to edit it. End jumps to **Save settings**. **Restore defaults** changes the
menu draft; **Save settings** applies it to the active session and persists it to
that session's profile. Escape or **Cancel** leaves everything unchanged.

The menu shows all feature switches and budgets with descriptions and validates
values before saving. It starts from the session's effective settings, including
any temporary overrides; saving deliberately makes all displayed values persistent.
A failed save leaves the menu open and the live settings unchanged. Concurrent
pipeline edits are detected so the menu cannot silently overwrite them. Changes
to other profile fields are preserved. Opening or editing this menu never submits
a prompt, contacts the model, or resumes pending tools.

`--plain` terminals get a numbered version of the same menu. Rich terminals use a
modal panel that restores the normal composer and scrollback on close. The command
line controls remain available for scripting.
