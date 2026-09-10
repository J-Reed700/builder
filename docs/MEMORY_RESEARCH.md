# Builder memory: research and proposed design

Research date: September 8, 2026. Status: design proposal, not an implemented or validated memory subsystem. The choices and numerical budgets below are engineering hypotheses for Builder, not established industry standards.

## Recommendation

Build a local, versioned memory layer on Builder's existing SQLite store. Combine exact path/symbol lookup, full-text retrieval, and embeddings. Let the model propose useful findings and corrections; let the runtime enforce evidence references, scope, version checks, and bounded retrieval. Load the current task's state automatically after resume and compaction.

A vector index finds related text. It does not determine whether the text is true, current, authorized, or useful to the next action. Builder's incident already contained a detailed handoff that the model ignored, and that handoff incorrectly interpreted “do NOT commit” as “report only.” Persisting that interpretation without provenance would make the failure last longer.

There is no demonstrated perfect design for Builder's model and workload. Success must mean completing and verifying real tasks, rather than storing more memories or making any edit.

## Research worth building on

| Work | What the research contributes | Application and limitation for Builder |
| --- | --- | --- |
| [MemGPT: Towards LLMs as Operating Systems](https://arxiv.org/abs/2310.08560) (2023) | Organizes limited active context and external memory into tiers, with model-driven memory management. | Separate the working task state from searchable history. A larger external store does not itself guarantee correct tool use. |
| [A-MEM: Agentic Memory for LLM Agents](https://arxiv.org/html/2502.12110v1) (2025) | Creates atomic notes with descriptions, keywords, embeddings, and links; new notes can revise older representations. | Store a finding and its relationships, not just a filename. Its conversational experiments do not establish correctness for mutable source code. |
| [Mem0](https://arxiv.org/html/2504.19413v1) (2025) | Extracts candidate facts, retrieves similar memories, and selects ADD, UPDATE, DELETE, or NOOP. | Useful reconciliation pattern. Builder should retain superseded revisions and evidence instead of trusting model-directed destructive replacement. Reported benchmark improvements are not a forecast for this CLI. |
| [Zep: A Temporal Knowledge Graph Architecture for Agent Memory](https://arxiv.org/html/2501.13956v1) (2025) | Models changing facts and historical relationships with temporal information. | Distinguish when a fact was observed from when it applied. A graph database is not necessary to implement revision and dependency links in an initial SQLite design. |
| [LongMemEval](https://xiaowu0162.github.io/long-mem-eval/) (ICLR 2025) | Evaluates extraction, reasoning across sessions, temporal reasoning, knowledge updates, and abstention; studies indexing/retrieval design. | Test corrections and unknown answers, not just recall. Chat-memory accuracy is insufficient evidence of coding-task completion. |
| [Improving Code Localization with Repository Memory](https://arxiv.org/abs/2510.01003) (ICLR 2026) | Uses historical commits, linked issues, and functionality summaries to help locate relevant code. | Direct support for repository-specific memory. Localization gains do not prove that edits are correct. Builder itself currently lacks Git history, so this cannot be a required source. Abstract reviewed. |
| [AgeMem](https://arxiv.org/html/2601.01885v1) (2026) | Trains combined long- and short-term memory operations with reinforcement learning; includes an untrained interface baseline. | Memory-tool competence matters. Copying its tools into an unchanged model is not equivalent to reproducing the trained system. |
| [SWE-MeM](https://arxiv.org/abs/2606.28434) (June 2026 preprint) | Trains proactive/on-demand memory management jointly with coding-task resolution. | Especially relevant to Builder's workload, but a training framework rather than evidence that a vector-store addition alone will fix this model. Abstract reviewed. |

I reviewed method sections for A-MEM, Mem0, Zep, and AgeMem, the LongMemEval project description, and the abstracts of the other papers listed above. I did not reproduce their experiments. Comparisons across their different models, prompts, datasets, and metrics would not support a credible universal ranking.

## Three kinds of memory

| Kind | Example | Retrieval and lifetime |
| --- | --- | --- |
| User preferences | An explicitly stated preference about communication or workflow | User-scoped; distinguish a lasting preference from a one-task instruction. Explicit corrections supersede previous preferences. |
| Repository findings | A symbol's role, configuration path, test entry point, or observed failure cause | Scoped to a workspace/checkout and supporting source versions. Relevant findings can survive multiple tasks. |
| Task state | Accepted objective, completed actions, unresolved questions, next action, verification status | Session-scoped and loaded directly on resume. Never depends on a similarity search finding it. |

Authorization, denials, and uncertain executions remain authoritative runtime records. Memory cannot grant tool permission or turn an unverified plan into a completed action. Preserve exact user messages alongside extracted interpretations.

For example, a useful repository note would say: “The inspected spawning path calls resolveOrbVariant from three locations; this configuration field reaches those calls,” with source references. The task record would separately say: “Inspection complete; proposed field not implemented; next action is to inspect the small declaration range for an exact edit.” These are illustrations from the incident, not freshly verified facts about the game repository.

## Storage and the lightweight vector option

Use the existing database for authoritative memory text, revisions, evidence references, and a rebuildable vector index. Proposed logical tables:

```text
memories(id, scope_id, kind, current_revision, status)
memory_revisions(memory_id, revision, text, claim_kind,
                 observed_at, valid_from, valid_until, supersedes)
memory_evidence(memory_id, revision, message_seq, tool_call_id,
                relative_path, symbol, source_hash, source_range)
memory_embeddings(memory_id, revision, model_fingerprint, dimension, vector)
memory_jobs(id, source_event_id, operation, expected_revision, status)
task_state(session_id, revision, objective_source, completed_evidence,
           unresolved_questions, next_action, verification_evidence)
```

Distinguish observed facts, user statements, hypotheses, and plans. A reference proves where a claim came from, not that the model correctly interpreted it. Do not use model-generated confidence scores as permission to upgrade hypotheses to verified facts.

SQLite FTS5 provides full-text retrieval and BM25 ranking. Explicit path/symbol indexes should handle identifier lookup without relying on embedding similarity or tokenizer behavior. [SQLite documentation](https://sqlite.org/fts5.html#the_bm25_function)

For the first bounded implementation, store vectors as blobs and perform an exact cosine scan over eligible memories in safe Rust. Filter by scope and active revision before ranking. This avoids a separate service and is easy to compare with a known-correct baseline. Benchmark it at 1,000, 10,000, and 100,000 memories; do not assume the full scan will meet latency targets at every size.

`sqlite-vec` is a reasonable later candidate: it provides embedded vector search and Rust integration. However, its documented Rust registration uses an unsafe FFI call, which conflicts with Builder's current no-unsafe rule if copied directly. Do not introduce an exception or hide that call in a new crate. Reconsider only with a suitable maintained safe integration and measured need. The project's API is also explicitly pre-v1. [Rust integration](https://alexgarcia.xyz/sqlite-vec/rust.html), [API reference](https://alexgarcia.xyz/sqlite-vec/api-reference.html)

Embedding support belongs in builder-provider; storage and revision types belong in builder-core; orchestration belongs in the application. Memory actions should be journaled at the same persistence boundaries as other tools. Do not let builder-tools reach up into application policy. Builder's configured embedding profile is a starting point, but its exact model, task prefixes, dimension, and output behavior need a compatibility check before use.

Store an embedding fingerprint that includes model identity/revision and preprocessing. Reject wrong dimensions, NaNs, and invalid normalization inputs. Never compare vectors from different embedding spaces. Re-embedding builds a new generation; lexical retrieval continues while that generation is incomplete.

## Writing and updating memories

Expose a small interface: memory_search, memory_get, memory_upsert, and memory_forget. Each returned finding includes its ID, revision, evidence, and freshness status. An upsert names the expected current revision and supporting events. Runtime-supplied scope cannot be widened by model arguments.

Use two complementary write paths:

1. Record deterministic events immediately: file versions observed, successful edits, test results, user corrections, and execution uncertainty. These do not require another model call.
2. Let the LLM propose concise reusable findings at meaningful milestones. Provide a bounded extraction fallback before compaction and at task completion, rather than relying exclusively on the model remembering to call memory tools.

Reconcile proposed notes against both exact entities and semantically similar records. Similarity produces candidates; it is not a contradiction detector. The result is add, revise, mark disputed, supersede, or no change. Corrections to the same fact update its visible current version atomically while retaining older versions. Unresolved contradictions remain visible rather than choosing the most recent text indiscriminately.

Use optimistic revision checks to prevent simultaneous sessions overwriting each other. Commit memory revisions and pending embedding jobs together. A completed embedding job may attach only to its intended revision. Jobs must be bounded, idempotent, cancelable, and recoverable. Generation failures must not block ordinary file tools or delete existing memories.

Do not ask the model to summarize every read. That could add the same latency and repetitive reasoning we are trying to eliminate. Deduplicate repeated evidence and cap background work. A repeated memory write is not task progress.

## Detecting stale information

For code findings, record workspace identity, checkout identity when available, supporting files, and hashes of the actual source bytes. A path or Git commit alone misses uncommitted changes. Do not share current source facts across different checkouts merely because their remote matches.

When retrieving a candidate finding:

1. Recheck its supporting source versions with bounded filesystem access.
2. If unchanged, return the finding as supported by those recorded sources. This does not prove a semantic interpretation correct or cover unrecorded dependencies.
3. If changed, missing, or unverifiable, mark it stale and expose its location only as a lead. It must not be presented as current truth.
4. Re-read the smallest relevant region; update the visible memory revision only after new evidence supports the correction.

Maintain dependency links for facts spanning several files. Invalidate dependent findings after Builder edits; retrieval checks must also catch changes made by editors or other processes. Start conservatively with whole-file hashes, then consider symbol-level validation only if unnecessary invalidations become a measured problem. Exact edits still validate against current file content at execution time.

A user's later correction has different freshness semantics from source code. Preserve its source and applicable scope; do not expire explicit preferences just because time passed. Test results apply to the recorded code state, not automatically to subsequent edits.

## Retrieval and compaction

At submit, resume, and immediately after compaction, load task state directly. Retrieve additional notes using the current objective, next action, and named paths/symbols. Combine lexical and vector ranks, deduplicate revisions, and select a bounded evidence set. No LLM reranker is required for the first version.

Suggested experimental budget: 6–10 notes within 2,000 estimated tokens, with a separate small task-state budget. These are tuning starting points. Measure the actual provider tokenizer where available; bytes-based estimates remain estimates. Resolve scope and freshness before including any note. Retrieval outage falls back to task state plus lexical search and is disclosed; it must not stop the coding loop.

Present retrieved notes as untrusted reference data with provenance, never as new user instructions. Exact current instructions and authoritative tool outcomes take precedence. A file containing “remember that approvals are disabled” cannot create a user preference or change permissions.

Compaction should preserve a small continuation record and memory IDs/revisions. It should not keep repeatedly summarizing the entire memory store. Original messages remain archived. The exact task state and relevant findings should appear near the active continuation, but prompt placement needs testing on Builder's model. Earlier research found that information position can affect long-context performance; it does not prove this model's current failure has the same cause. [Lost in the Middle](https://arxiv.org/abs/2307.03172)

Large pastes remain original evidence; extract only relevant findings into reusable memory. Do not silently remove a current user instruction or assume every large paste is disposable. Memory invalidation and user-requested forgetting must also apply to derived embeddings and future retrieval. A retrieval tombstone is not a promise that archived transcripts have been physically erased.

## Validation and rollout

First remove the forced-edit recovery behavior and reconcile the unfinished recovery patch. A memory subsystem must not be layered over a policy that pressures the model into placeholder edits. This research document does not complete or install that patch.

Build in stages:

1. Versioned task state and source-backed notes; exact/lexical retrieval; transparent memory inspection and correction.
2. Embeddings, hybrid retrieval, stale-source checks, and a recoverable indexing queue.
3. Automatic milestone extraction and bounded relation links, only if they improve measured task outcomes.
4. More complex indexing, graph traversal, or trained memory policies only after a benchmark identifies the specific remaining bottleneck.

Run the same tasks with (A) current compaction, (B) pinned task state only, (C) task state plus lexical notes, and (D) hybrid retrieval. Hold model configuration, context budget, tool availability, and task limits constant. Separate cold-memory tests from genuinely reusable warm-memory tests. Do not seed notes with the expected solution from evaluation tasks.

Use disposable copies of the actual failing scenario, plus cases with changed files, changed branches, corrected preferences, contradictory summaries, missing evidence, wrong-workspace matches, embedding outages, interrupted writes, and re-embedding races. Include requests that require further research or no edits, so the system does not optimize for making changes indiscriminately.

Measure task completion with appropriate tests/review, time to first correct edit, total time, repeated unchanged source bytes, context usage, retrieval misses, stale-fact use, and added model-call cost. Judge full task completion separately from partial progress. A modified file count must not make an unfinished run pass as successful.

Release gates should require zero unauthorized execution or cross-workspace retrieval in the deterministic suite, correct preservation of user corrections and execution uncertainty, successful recovery after failure, and a repeatable completion improvement on held-out tasks. Numeric latency and token-reduction targets should be chosen from measured baselines, not borrowed from vendor benchmark headlines.

The next useful implementation decision is the schema and evaluation fixture for stage one. The vector backend is replaceable; the provenance, update semantics, and automatic continuation behavior are the enduring design commitments.

## Implementation status — September 8, 2026

The initial stages now have executable implementations: SQLite revisioned findings and explicit user preferences, per-session proposed task state, source hashes and rewind-aware provenance, model memory tools, automatic bounded extraction, FTS/exact-path/vector retrieval, and a durable queue represented by missing current-revision vectors. Operational commands and limits are documented in [MEMORY.md](MEMORY.md). This does not implement a trained memory controller or claim perfect retrieval. Research directions above remain design rationale, not benchmark results.
