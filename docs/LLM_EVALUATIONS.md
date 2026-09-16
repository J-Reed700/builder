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
| `BUILDER_EVAL_MODE` | `compacted` | Seed a checkpoint, or use `history` with the same historical reads and finding content |
| `BUILDER_EVAL_REPORT` | no report file | New JSON report destination; existing files are not overwritten |

Each trial has a 20-agent-round limit. The evaluation caps request timeout at 60 seconds, idle timeout at 45 seconds, and transport attempts at one. Normal bounded agent output-limit recovery and compaction remain enabled according to the profile/runtime. The wall-clock deadline includes that work. A full default run is bounded by five trial deadlines plus fixture/report overhead; repetitions multiply this bound.

`history` is an alternate starting projection, not “memory disabled.” The normal agent may still compact or trigger investigation recovery. Profile temperature and completion settings remain intact; repeated trials measure observed variability rather than deterministic seeded model behavior. Use fresh reports and identical profiles/budgets for comparisons. For release decisions use multiple repetitions, paired cases, held-out variations, and a larger workload set. Four successful trials are only a smoke result.

## Grading and reports

No LLM judge can award a pass. The grader parses complete JSON structures and compares every key/value, allowing formatting differences. It also requires the requested final JSON answer, a successful agent return, a non-pending session, and read-back of both files after their last successful mutation. A timeout, round-limit stop, partial edit, missing verification, unsupported answer, or disallowed action request fails the trial. A model's assertion that it finished cannot override these checks.

The negative grader tests include unchanged/partially changed files, unrelated setting changes, stale values, invalid data, invented answers, and prose claiming success. They establish that the grader detects these errors; they are not evidence of model quality.

Reports contain pass/fail reasons, elapsed time, model request count including compaction, serialized prompt bytes, tool call/failure counts, compaction count, and repeated identical read-output bytes within the live portion of the trial. This repetition metric does not count partially overlapping ranges or seeded historical reads. Byte counts are not provider token usage. Report files omit endpoint URLs, credentials, headers, and raw transcripts. Report errors may include diagnostics derived from the synthetic fixture.

The final-answer contract is intentionally strict JSON. A correct file edit accompanied by prose can fail that contract; inspect the separate failure reasons before diagnosing a reasoning failure. The live suite runs all selected trials and writes the report before returning a nonzero test result for any failed trial.

`live_progress.rs` remains a separate opt-in diagnostic for a one-line code fixture and a private historical replay. It is not interchangeable with this objective suite. The replay must now finish its run as well as make multiple edits, but it has no project-wide semantic correctness oracle and must not be cited as proof of a correctly completed real task.

## Memory comparison

Set `BUILDER_EVAL_MEMORY=off` (default) or `on` for paired runs of the same suite. On mode uses the configured memory embedding profile, if available. Schema-v2 reports record this flag. `model_requests_including_compaction` also includes memory extraction requests; embedding HTTP requests are separate and are not counted as chat prompts. Elapsed time includes memory work. Use distinct report paths and repeat each mode; a single small-suite result is not statistical evidence.

Generated reports are local run artifacts and should not be committed. Preserve the inputs and grading contracts when comparing models or runtime changes.
