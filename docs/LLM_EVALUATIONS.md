# Builder behavioral evaluations

Builder has two separate test layers. `resilience.rs` uses a controlled HTTP server to verify persistence, protocol, permissions, interruption, and recovery. `llm_evals.rs` runs the real agent against a configured model and grades what it actually produced. Passing the first layer does not imply that a model can complete tasks.

## Current scenarios

| Case | Failure being tested | Objective completion criteria |
| --- | --- | --- |
| `resume_findings` | Restarting exploration despite a usable handoff; confusing no-commit with no-edit | Correct changes to both configuration files, preservation of all other values, read-back verification, accurate final answer, and a completed session |
| `stale_handoff` | Overwriting a newer source value using historical findings | Same criteria, including preservation of the current damage value absent from the old handoff |
| `user_correction` | Continuing an obsolete plan after a follow-up | Apply the corrected value to both files and report the actual resulting values |
| `invalid_tool_loop` | Three persisted invalid research finish calls before restart | Recover, complete both edits, verify them, and finish without manual steering |
| `unknown_read_only` | Inventing a value from unsupported memory, or making unnecessary edits | Return JSON null for the absent field, leave files unchanged, and make no disallowed tool requests |

These are deliberately small configuration-editing tasks with mechanically checkable answers. They exercise the actual provider, agent, tools, database checkpoint, and disk reopen. They are not a comprehensive benchmark of software engineering or a validation of the proposed vector memory system. Twelve historical reads reproduce the investigation-recovery condition without spending model calls. Handoffs include historical evidence, not hidden grader output. The expected results are held in the test process, not passed to the model beyond the user's actual requirements.

## Run

Normal, network-free checks include negative tests of the graders themselves:

```sh
cargo test --workspace --locked
```

Run the live suite explicitly; it sends synthetic fixture content to the selected configured endpoint and may incur its normal inference costs:

```sh
BUILDER_LIVE_CONFIG_HOME='/path/to/builder/config-directory' \
BUILDER_EVAL_REPORT='/path/to/new-report.json' \
cargo test --test llm_evals live_behavior_suite -- --ignored --nocapture
```

The profile defaults to Builder's configured default. `BUILDER_EVAL_PROFILE` selects another saved profile. No credentials or real repository files are copied into fixtures. Every run gets a disposable workspace and database. The approval callback permits edits only to the two named fixture files; it denies all shell commands and every mutation in the read-only case. Generated code is never executed. The fixtures are deleted when the run ends.

Options:

| Variable | Default | Meaning |
| --- | --- | --- |
| `BUILDER_EVAL_CASE` | all five | Select one exact case name; unknown names fail |
| `BUILDER_EVAL_REPEATS` | 1 | Independent fresh trials, 1–10 |
| `BUILDER_EVAL_TIMEOUT_SECS` | 180 | Per-trial wall-clock ceiling, 1–600 seconds |
| `BUILDER_EVAL_REQUEST_TIMEOUT_SECS` | 60 | Per-request deadline, 1–600 seconds |
| `BUILDER_EVAL_IDLE_TIMEOUT_SECS` | 45 | Per-request idle deadline, 1–600 seconds |
| `BUILDER_EVAL_MODE` | `compacted` | `compacted`: handwritten checkpoint; `history`: historical reads; `generated`: three actual model-generated compactions with continuation and restart |
| `BUILDER_EVAL_REPORT` | no report file | New JSON report destination; existing files are not overwritten |

Each continuation has a 20-agent-round limit (one per trial normally, three in `generated` mode). The evaluation defaults to a 60-second request timeout and 45-second idle timeout, with transport attempts capped at one. The two timeout overrides are recorded in reports; use identical budgets for comparisons. Normal bounded agent output-limit recovery and compaction remain enabled according to the profile/runtime. The wall-clock deadline includes that work. A full default run is bounded by five trial deadlines plus fixture/report overhead; repetitions multiply this bound.

`history` is an alternate starting projection, not “memory disabled.” The normal agent may still compact or trigger investigation recovery. Profile temperature and completion settings remain intact; repeated trials measure observed variability rather than deterministic seeded model behavior. Use fresh reports and identical profiles/budgets for comparisons. For release decisions use multiple repetitions, paired cases, held-out variations, and a larger workload set. Four successful trials are only a smoke result.

## Generated compaction evaluation

To test the summarizer itself as well as continuation:

```sh
BUILDER_LIVE_CONFIG_HOME='/path/to/builder/config-directory' \
BUILDER_EVAL_MODE=generated \
BUILDER_EVAL_REQUEST_TIMEOUT_SECS=180 \
BUILDER_EVAL_IDLE_TIMEOUT_SECS=90 \
BUILDER_EVAL_TIMEOUT_SECS=600 \
BUILDER_EVAL_REPORT='/path/to/new-generated-report.json' \
cargo test --test llm_evals live_behavior_suite -- --ignored --nocapture
```

Each case runs three actual `Agent::compact` calls against the selected model, reopens the database after every checkpoint, and runs the agent between compactions. The first two continuations inspect without editing; the third implements the original request. No handwritten handoff is injected. Repeated real fixture reads supply compressible context; twenty informational user rounds force the bounded source-backed memory path. The harness checks that bounded memory was actually produced and archived history is unchanged. Final behavior, rather than equality of all active user messages, determines whether requirements survived. The correction case starts with the old value before receiving the correction; the stale case changes source externally after the final summary.

The same objective final-state and verification graders apply. Inspection-only turns deny mutations and check file preservation. A skipped compaction, failed continuation, history corruption, timeout, or incomplete cycle fails the trial. Reports include completed continuation cycles; synthetic padding reads are excluded from model tool/read metrics. The timeout covers all three compactions and continuations together. Schema-v5 reports use `instruction_retention: bounded_source_quotes`. Earlier v4 preserved all user messages; v3 relied on summaries. Results are not interchangeable across these designs.

For diagnosis, set `BUILDER_EVAL_TRACE_DIR` to a new directory. The harness saves the active messages after each checkpoint and the original history after each trial, including failed trials. These opt-in files contain raw synthetic conversation and tool data; normal score reports still omit transcripts. The directory must not already exist.

Active instruction memory is bounded to 32 exact source quotations and 8192 serialized bytes. Small user histories (up to 4096 serialized bytes) retain their messages directly. Once extraction is needed, pins persist by default; model-selected retirement requires an exact later user passage. Original user messages remain on disk, with stable `seq` IDs for history retrieval. Invalid quotes, unknown sources, old retirement evidence, oversized memory, or generation failures leave the active checkpoint unchanged. This bounds ordinary historical growth, but cannot guarantee that arbitrarily many simultaneously applicable instructions fit. See [CONTEXT_MEMORY.md](CONTEXT_MEMORY.md) for limits and semantic failure modes.

This exercises repeated summary loss on small synthetic tasks. It does not cover every repository, long-running edit/test workflow, model, or failure mode. Use multiple independent trials to assess variability; a single successful run is a smoke check.

## Many-turn compaction scale and recall

`cargo test --test compaction_scale -- --nocapture` builds synthetic conversations with 16, 64, 256, 512, and 1024 detailed user/assistant rounds. A deterministic source selector and tiny mock handoff test context growth: every size must compact below 6000 estimated tokens, while all originals remain unchanged. This is a size/mechanics test, not evidence of model selection accuracy.

For actual summarization and recall over 128 synthetic information requests:

```sh
BUILDER_LIVE_CONFIG_HOME='/path/to/builder/config-directory' \
BUILDER_EVAL_REPORT='/path/to/new-many-turn-report.json' \
cargo test --test compaction_scale live_many_turns_preserve_early_middle_and_recent_findings -- --ignored --nocapture
```

The live test generates a real summary, reopens the checkpoint, and asks the model for exact findings from early, middle, and recent rounds, plus an original user constraint and JSON-only output. Queried findings originate in assistant messages outside the retained tail, so keeping user requests alone cannot satisfy the grader. It compares complete parsed JSON with known fixture facts and records size measurements and the synthetic handoff. Tools and optional cross-session memory are disabled; production source-backed compaction memory remains enabled. This is one compaction of a long conversation, not a repeated-compaction endurance test; token counts use Builder's estimator rather than provider usage.

## Repeated source-backed memory and retrieval

```sh
cargo test --test layered_memory
BUILDER_LIVE_CONFIG_HOME='/path/to/builder/config-directory' \
BUILDER_EVAL_REPORT='/path/to/new-layered-report.json' \
cargo test --test layered_memory live_repeated_memory_correction_retrieval_and_abstention -- --ignored --nocapture
```

The offline tests validate durable pins, later corrections, incremental source processing, exact archive retrieval, absent search results, rewind filtering, and atomic rejection of fabricated quotations. They use a deterministic selector and do not establish model judgment.

The live test seeds 48 information rounds (override with `BUILDER_EVAL_ROUNDS=128`, accepted range 40–512), generates memory and summaries three times, reopens every checkpoint, and continues through the real model between compactions. A correction arrives after the first checkpoint. Complete JSON checks require the updated value, original no-deployment restriction, and original output schema. A final probe requires actual `history_search` and `history_read` calls for an older record selected after compaction, plus null for a never-provided record. This is a bounded synthetic smoke evaluation, not LongMemEval or comprehensive coverage. Model selection, supersession, and retrieval can still fail.

A separate fast live diagnostic isolates history-tool transport from summarization:

```sh
BUILDER_LIVE_CONFIG_HOME='/path/to/builder/config-directory' \
BUILDER_EVAL_REPORT='/path/to/new-history-tool-report.json' \
cargo test --test layered_memory live_history_tools_accept_parameterized_operations -- --ignored --nocapture
```

It deliberately uses a handwritten checkpoint with an omitted fact and requires real parameterized `history_search` and `history_read` calls plus the exact answer. This is a tool/schema diagnostic, not a compaction accuracy test. The offline schema tests require operation-first property order after serialization, correct enabled-operation filtering, and rejection of missing or cross-operation fields. Both transport and semantic failures remain failures in live reports; failed runs are not replaced or overwritten by reruns.

## Grading and reports

No LLM judge can award a pass. The grader parses complete JSON structures and compares every key/value, allowing formatting differences. It also requires the requested final JSON answer, a successful agent return, a non-pending session, and read-back of both files after their last successful mutation. A timeout, round-limit stop, partial edit, missing verification, unsupported answer, or disallowed action request fails the trial. A model's assertion that it finished cannot override these checks.

The negative grader tests include unchanged/partially changed files, unrelated setting changes, stale values, invalid data, invented answers, and prose claiming success. They establish that the grader detects these errors; they are not evidence of model quality.

Reports contain pass/fail reasons, elapsed time, model request count including compaction, serialized prompt bytes, tool call/failure counts, compaction count, and repeated identical read-output bytes within the live portion of the trial. This repetition metric does not count partially overlapping ranges or seeded historical reads. Byte counts are not provider token usage. Report files omit endpoint URLs, credentials, headers, and raw transcripts. Report errors may include diagnostics derived from the synthetic fixture.

The final-answer contract is intentionally strict JSON. A correct file edit accompanied by prose can fail that contract; inspect the separate failure reasons before diagnosing a reasoning failure. The live suite runs all selected trials and writes the report before returning a nonzero test result for any failed trial.

`live_progress.rs` remains a separate opt-in diagnostic for a one-line code fixture and a private historical replay. It is not interchangeable with this objective suite. The replay must now finish its run as well as make multiple edits, but it has no project-wide semantic correctness oracle and must not be cited as proof of a correctly completed real task.

## Memory comparison

Set `BUILDER_EVAL_MEMORY=off` (default) or `on` for paired runs of the same suite. On mode uses the configured memory embedding profile, if available. Schema-v2 reports record this flag. `model_requests_including_compaction` also includes memory extraction requests; embedding HTTP requests are separate and are not counted as chat prompts. Elapsed time includes memory work. Use distinct report paths and repeat each mode; a single small-suite result is not statistical evidence.

Generated reports are local run artifacts and should not be committed. Preserve the inputs and grading contracts when comparing models or runtime changes.
