# Tool recovery and optional-memory reliability

These changes address the repeated invalid research calls and auxiliary-memory delays observed while resuming session `4263c02e`.

- Research results expose their durable `record_id` for subsequent evidence references; stale references still fail validation.
- Research requests and artifact selectors use separate JSON Schema branches. Each branch declares its own required fields and rejects extra properties. `finish` cannot advertise `verification_ids`, and it requires `explanation`.
- Three consecutive tool errors trigger an explicit checkpoint recovery. The original journal and call IDs remain intact. Recovery allows at most three corrective rounds before pausing honestly; existing investigation recovery retains its own bound. Permission denials and uncertain execution still cannot be bypassed.
- Extraction receives only eligible fresh source evidence and current findings. Batches without eligible reads make no model call. The durable cursor distinguishes attempted from completed extraction, preventing a bad batch from being retried on every restart. At most one extraction generation runs per post-answer idle pass; new source reads can supply later evidence.
- Automatic extraction and indexing are cancellable idle work after a final answer. Foreground requests and compaction never await them, and new user work cancels maintenance before contacting the chat model.
- Automatic foreground packets use lexical retrieval. Explicit embedding requests retain their deadline and circuit breaker. Missing vectors remain queued, with at most two indexed records per idle pass.
- Runtime guidance preserves the user's requested final-answer format through recovery.

Freshness is still checked before saving and retrieving findings and before accepting research verification. No original conversation rows are removed, and optional memory failures never become evidence of successful work. Skipping failed derived extraction can leave fewer automatic findings; the original source/history and explicit memory tools remain available.

## Validation

Formatting, strict Clippy, and all 135 non-live workspace tests passed. Behavioral regressions cover persisted invalid-call loops, recovery exhaustion, restart after failed extraction, stale batches, immediate completion, hanging embeddings, and operation-specific schemas. Existing permission, uncertain-execution, freshness, and completion-gate tests also pass.

Live results are recorded separately below. These are small synthetic fixtures against the configured Qwen model, not a general reliability guarantee or a new run of the original Arena session.

## Live observations

The configured Qwen3.8-27B-Q5_K_S.gguf completed all five behavior scenarios across the final selected trials: resume, stale handoff, user correction, absent read-only field, and recovery from three persisted invalid finish calls. The invalid-call case completed both edits, read-back verification and its final answer without new tool failures or manual steering.

The first resume trial failed its strict JSON-only answer requirement despite correct edits. After strengthening format guidance, the rerun passed. Both active-plan Rust contract trials completed with verified outcomes and passed independent held-out tests. The optional-contract trial initially made an invalid evidence reference and recovered; after exposing record IDs, its rerun completed with no tool errors or denials.

The actual embedding service still returned HTTP 502. This build handles that outage; it does not repair the remote service. These are small single-trial checks, not a model reliability guarantee. See the [machine-readable results](reliability-live-results-2026-09-08.json) for timings, failures and reruns.

### Successful memory retrieval loops (2026-09-08)

Session `1783c0f4` repeatedly returned stale location hints or empty lexical
results while varying query wording. No source inspection followed. Successful
`memory_search` and `memory_get` results were missing from the investigation
counter, so neither tool-error recovery nor investigation recovery activated.

Both now count toward the existing 12-inspection recovery threshold. Recovery
preserves archived originals and offers at most eight guided rounds before an
honest pending pause. Guidance explicitly directs repeated/stale retrieval to
current source and permits status answers without edits. The automatic memory
packet also explains that retrieval has already occurred and stale paths need
source inspection. This guard works with local, remote, and lexical retrieval;
changing embedding backends alone does not fix an agent control-flow loop.

The behavioral regression seeds successful stale retrievals, reopens the store,
and checks both source-read/final-answer recovery and a model that ignores the
guidance. It verifies the hard bound, retained original history, read-only
behavior, and pending state when recovery fails.

The final guided round now explicitly requests a conclusion without more tool
calls, separating verified results from unfinished work. Tools and authorization
checks remain intact; a model that ignores the instruction still pauses rather
than being marked complete. In the first live replay, Qwen escaped memory
retrieval and read the Arena setting but exhausted the inspection budget without
answering. That failure motivated the final-round conclusion instruction.

The revised live Qwen replay finished the saved status question after reading the
current Arena configuration and spawn call sites. The first bounded-pause result
is retained as a failure, not counted as a successful answer. Testing used an
isolated SQLite backup because the original session was locked by another
process; it used current `drift-temp` files under read-only approval.

Following review, the guards classify decoded `Action` variants, not tool-name
strings or user wording. Database schema 5 persists `ToolOutcome` with each tool
result in the same transaction. Failure recovery reads that enum; inspection
recovery resets on a recorded file change, not printed success/error prefixes.
Direct writes compare bounded source hashes before/after successful execution;
candidate applications already validate nonempty changing patches. Legacy
outcomes remain unknown rather than inferred from text. These runtime rules
contain no Qwen, llama.cpp, model-name, or endpoint-specific branches. Live
behavior has been checked with the user's Qwen profile, not every model.

The no-progress recovery prevents the failure observed in session `56669e0d`:
after the configured threshold, broad `list_files` and `search` calls are not
advertised again until real file progress or a new user instruction. Recovery no
longer summarizes or archives the active conversation; only the configured context
threshold or explicit `/compact` can create a summary checkpoint. The
enforced conclusion no longer includes the contradictory claim that tools remain
available. Conclusion output is buffered until accepted; literal `<tool_call>`
markup and structured calls are neither displayed nor executed, and receive one
bounded plain-prose repair attempt before an honest runtime fallback. These guards
inspect runtime state and response structure, not the configured model name.

## Configurable run budget

The `35e96f50` transcript completed the shield status question after 14 actions,
but the subsequent comment-cleanup task consumed the old hardcoded 20-round
budget debugging its generated script. This was budget exhaustion, not the
memory-search loop. The default is now 100, with a persisted `Agent rounds per
run` control in `/settings` (1–1000), applied immediately. Existing explicit
`--max-rounds` invocations remain supported. Models receive a remaining-budget
notice during the last five rounds. This does not establish that the generated
cleanup script is correct or that repository-wide cleanup has completed.

The liveness limits are also profile settings rather than agent constants. The
default no-progress ceiling is 100 calls, and progress/failure recovery retains the
full active transcript. `/status` shows these effective limits alongside the
configured context window and compaction threshold.

A behavioral test executes productive work past round 20, checks that exhaustion
leaves the final requested call unclaimed, reopens the database, and confirms
explicit continuation executes it once and finishes. Configuration tests cover
legacy defaults, range validation, and persistence. Loop guards remain enabled.
