# Builder capability audit and research roadmap

Research and repository audit date: September 10, 2026.

## Verdict

Builder has stronger persistence, recovery, provenance, stale-memory rules, and
current-code retrieval than many coding harnesses, but it is not yet the final
state of a cutting-edge repository intelligence system. This implementation adds
a content-addressed code index with structural chunks, exact and BM25 retrieval,
reference expansion, local dense vectors, atomic generations, event-driven and
periodic refresh, query-time source validation, abstention, ranking explanations,
and bounded automatic context. The next gains require measuring retrieval quality,
adding compiler-resolved graphs and repository history, and evaluating stronger
embedders and rerankers on real issue-to-edit tasks.

More agent rounds or a larger context window will not substitute for measuring and
improving which source lines reach the model.

This document distinguishes deployed behavior, measured evidence, and research
hypotheses. No paper result is treated as a performance claim for Builder.

## What is deployed now

| Capability | Current implementation | Assessment |
| --- | --- | --- |
| Durable execution | Tool calls, results, uncertainty, resume, rewind, and compaction checkpoints are journaled. Originals remain available. | Strong foundation. |
| Source-backed memory | Findings use immutable revisions, workspace scope, source hashes, rewind-aware evidence, and tombstones. Changed evidence is returned only as a stale location lead. | Unusually careful and worth retaining. |
| Local embeddings | Pinned `all-MiniLM-L6-v2` ONNX files, offline CPU inference, true bounded batch generation, 384 normalized dimensions, content/model-fingerprinted vectors, and lexical fallback. | Operational, small, and general-purpose rather than code-optimized. Its input is limited to 256 tokens. |
| Retrieval | Memory findings use fused lexical/dense ranking. Code search separately fuses exact paths/symbols, FTS5/BM25, declared-symbol reference expansion, and dense rank; it applies a semantic floor, path diversity, source-hash validation, ranking explanations, and abstention. | Strong deterministic baseline. It still lacks a learned reranker and calibrated repository-specific threshold. |
| Code indexing | Complete checkout snapshots use language-aware declaration chunks, file/content hashes, atomic SQLite generations, FTS5, reusable dense vectors, recursive filesystem events, debouncing, periodic reconciliation, and synchronous query refresh. | Freshness and failure behavior are implemented. Capture still performs complete bounded scans instead of path-delta parsing. |
| Structural navigation | The durable index extracts declarations and identifier references for common languages. Rust also gets `syn` candidates; optional LSP calls provide bounded definition/reference/diagnostic queries. | Broader and persistent, but still identifier-based rather than a compiler-resolved call/definition-use/data-flow graph. |
| Investigation and verification | Typed plans, observations, hypotheses, disposable candidate tests, real-workspace verification, independent review, completion gates, history retrieval, and learned procedures. | Sophisticated controls, but the large research-operation schema can burden smaller local models and has not been validated on a broad task suite. |
| Context management | Exact latest request, recent complete tool groups, deterministic read inventory, explicit manual/pressure compaction, and retained originals. | Safe. Compression timing is static and summary quality is only lightly measured. |
| Evaluation | Extensive deterministic reliability tests and four small live-model fixtures. | Good reliability coverage; insufficient evidence for model quality or retrieval quality. |

## Live checks from this audit

The installed configuration uses local embeddings and reported no embedding error.
The pinned model files were present. A live Builder-checkout memory search returned
hybrid results, and the installed-model integration test generated a normalized
384-dimensional vector, persisted it, reopened the database, and retrieved the
semantically related note without network access.

The Drift checkout exposed the quality gap. It had seven current memory records.
The query `Arena shield powerup drop rate boss collision` returned hybrid results,
but the packet contained unrelated orb-registry and comment-cleanup findings and no
shield-drop finding. Several changed records were correctly marked stale. This shows
that generation, storage, and invalidation work while recall coverage, relevance,
and abstention do not yet work well enough.

The existing one-shot live evaluation improved from 1/4 cases with memory disabled
to 3/4 with memory enabled, while elapsed time rose from 111.7 to 230.2 seconds.
That is encouraging smoke evidence, not a statistically useful comparison. It did
not evaluate a populated code index or a warm repository-memory workload. The new
index has deterministic stale-race, atomic-generation, bound-failure, abstention,
structural extraction, watcher, and local-model hybrid tests. A Drift smoke scan
indexed 3,624 files into 23,217 chunks; that proves operation and scale, not
retrieval quality.

## What recent research changes

### Retrieval must be evaluated on real agent needs

[CORE-Bench](https://arxiv.org/abs/2606.11864) evaluates code understanding,
issue-to-edit localization, and broader context retrieval against concrete
repository states. Its August 2026 revision reports a large drop from traditional
code search to agentic repository retrieval. In its representative results,
Qwen3-Embedding-0.6B scored 66.9 NDCG@10 on its level-1 task but 17.0 on
issue-to-edit localization. Fine-tuning the 0.6B model on pull-request descriptions,
patch-aligned positives, and repository-local hard negatives increased the latter
to 26.5. The implication for Builder is that selecting a model by MTEB or
CodeSearchNet alone is inadequate; evaluation needs actual issue-to-edit and
supporting-context labels.

[SWE-Explore](https://arxiv.org/abs/2606.07297) evaluates 848 issues across ten
languages under a fixed line budget. It finds that coverage, ranking, and context
efficiency track downstream repair behavior, and that line-level coverage and
ranking distinguish the best explorers. Builder should therefore measure selected
files and line ranges, not only final pass/fail or whether an embedding request
succeeded.

[ExecRetrieval](https://arxiv.org/abs/2609.01865), submitted September 1, 2026,
adds execution-verified, single-edit buggy variants next to correct implementations.
Its best hosted retriever reached 100% at rank 10 but only 33.1% at rank 1; 91.5% to
99.4% of leading systems' rank-1 misses were the paired buggy variants. Dense
similarity is therefore a candidate generator, not a correctness oracle. Builder
must carry source state, structure, tests, and verification through retrieval and
selection.

### Structure complements lexical and dense retrieval

[ARISE](https://arxiv.org/abs/2605.03117) adds a multi-granularity program graph
with statement-level definition-use edges and exposes data-flow slicing as an agent
primitive. On its reported Qwen2.5-Coder-32B/SWE-bench Lite setup, it improved
Function Recall@1 by 17 points, Line Recall@1 by 15 points, and repair Pass@1 by
4.7 points over its SWE-agent baseline.

[RANGER](https://arxiv.org/abs/2509.25257) combines hierarchical and cross-file
repository graphs, descriptions, embeddings, direct entity lookup, MCTS-guided
natural-language graph exploration, and BM25. Its reported results favor the
combined graph and lexical approach over embedding-only baselines. A simpler
Builder implementation should begin with deterministic graph expansion and earn
the complexity of MCTS through ablation.

Aider's current [repository map](https://github.com/Aider-AI/aider/blob/main/aider/website/docs/repomap.md)
is also a useful production baseline: it extracts definitions and references,
ranks the dependency graph, and fits selected signatures into a token budget.
Builder currently lacks even this always-available repository skeleton outside
Rust.

### Repository history is useful memory

[Improving Code Localization with Repository Memory](https://arxiv.org/abs/2510.01003)
builds episodic memory from commits and linked issues and semantic memory from
functionality summaries of active code. It reports improved LocAgent localization
on SWE-bench Verified and Live. Git history is immutable evidence, but a historical
symbol or fix still needs mapping to and validation against the current worktree.

Builder's current memory records what its model happened to extract after a tool
batch. It does not ingest commits, diffs, issue text, co-change relationships,
reverts, or file churn. Those signals can provide useful priors without claiming
that an old implementation is current.

### More context and more attempts need selection

[NoLiMa](https://arxiv.org/abs/2502.05167) shows substantial degradation on
associative retrieval as context grows even for models advertised with long
windows. Ten of twelve tested models fell below half their short-context baseline
at 32K. A configured 160K window is capacity, not proof that 160K of raw history
helps the model.

[FastContext](https://arxiv.org/abs/2606.14066) separates repository exploration
from the solver and returns concise file and line citations. Its specialized
explorers reportedly improved end-to-end resolution by up to 5.5% while reducing
main-model tokens by up to 60%. Builder can test the architectural idea before
training a separate model: run an isolated, bounded explorer context and give the
solver only its cited regions plus exact source hashes.

[SWE-MeM](https://arxiv.org/abs/2606.28434) trains models to decide when, what, and
how to compress. It is evidence that static threshold compaction is not the final
design. Adding an untrained `compress` tool to Builder would not reproduce the
paper. A safe near-term version is density-aware observation folding with exact
task state and reversible original history, evaluated against the current static
checkpoint.

[Scaling Test-Time Compute for Agentic Coding](https://arxiv.org/abs/2604.16529)
represents, selects, and reuses long rollout summaries rather than merely sampling
more full trajectories. It reports meaningful gains on SWE-bench Verified and
Terminal-Bench. Builder's disposable candidate runner is a good base for a bounded
hard-task mode that compares two to four independent localization/patch attempts
using tests and structured trajectory summaries.

### Complexity itself can hurt

[Agentless](https://arxiv.org/abs/2407.01489) is an older but still relevant
control: a simple localization, repair, and validation pipeline outperformed more
complex open-source agents in its evaluation. Builder should expose the smallest
phase-appropriate action set and compare every new mechanism against a simple
baseline.

## Recommended implementation order

### Impact-ranked execution sequence

The requested execution order is: (1) retrieval evaluation and then measured
local embedding/reranking improvements; (2) compiler-resolved code navigation;
(3) isolated exploration with source-validated results; (4) incremental index
updates; (5) richer Git history; (6) competing verified fixes. Evaluation comes
first as the acceptance gate for retrieval changes, not as a claim that a test
harness alone improves the model. This order is an engineering judgment about
the observed localization failures, not a measured ranking of all techniques.

The initial [evaluation runner](RETRIEVAL_EVALUATION.md) now measures unique-file
ranking and unioned source-line coverage against externally supplied labels.
It compares lexical/syntax graph variants and optionally the installed local
embedder, rejecting incomplete semantic coverage and changed labeled sources.
A reviewed multi-repository corpus, alternate embedders, and learned reranking
remain outstanding.

Status on this branch: P0 item 2 has a complete-generation foundation; item 3 has
filesystem events, fallback reconciliation, status, and retrieval-time validation
but still rebuilds a complete bounded snapshot rather than prioritizing path deltas.
Item 4 has exact, BM25,
syntax-symbol graph, Git-history and dense candidate stages with source validation,
fusion, diversity and abstention. Item 1 now stores bounded retrieval telemetry
and has three deterministic held-out issue-to-file cases, but still needs a real
multi-repository corpus and downstream edit labels. Item 5 remains an empirical
MiniLM/Qwen/code-embedder bakeoff. P1 item 6 has Tree-sitter declarations and an
exact identifier graph but no compiler-resolved call/data flow; item 7 indexes
commit subjects and changed paths but not diffs/blame/reverts; item 8 is a typed
durable-outcome phase router; item 9 actively probes and fingerprints ordinary,
JSON, native-tool and parallel-tool behavior but not the full sampling grid; item
10 adds configurable score/margin abstention and diagnostics but lacks held-out
per-model calibration. P2 already has bounded artifact subanalysis, disposable
candidate tests and evidence-backed learned procedures, while autonomous explorer
rollouts and trajectory selection remain unimplemented. The list is kept here so
a successful index build is not mistaken for completion of the measured roadmap.

### P0: measurement and retrieval foundation

1. **Add retrieval telemetry and held-out evaluation.** Record the ranked file,
   symbol, and line candidates shown to the model; retrieval source; raw and fused
   ranks; stale suppressions; time to first correct edit; repeated read bytes;
   tool-schema/model failures; token use; and final verification. Create a small
   Builder-specific issue-to-edit suite from pre-change repository snapshots, then
   add CORE-Bench and SWE-Explore subsets. Run repeated paired trials.
2. **Build a content-addressed code index.** Parse source into symbol-sized chunks
   with path, language, byte/line span, signature, imports, definitions,
   references, file hash, worktree identity, and index generation. Index exact
   identifiers and FTS immediately. Add dense vectors as a rebuildable artifact.
   Atomically activate a complete generation; never mix branch/worktree scopes.
3. **Make updates immediate and observable.** Watch filesystem changes, reconcile
   Git HEAD and uncommitted content, tombstone removed chunks, and prioritize
   changed/opened files. Recheck every selected chunk's digest at retrieval time.
   Show indexed, pending, failed, stale, model fingerprint, and last-refresh state
   in `/memory` or a repository-intelligence menu.
4. **Use staged retrieval.** Extract exact paths, identifiers, stack frames, error
   strings, and config keys from the request. Fuse exact/FTS and dense candidates;
   expand one or two graph hops; rerank a small set; enforce path and line
   diversity; and return a token-budgeted evidence packet. The router should be
   able to abstain and request agentic exploration. Surface why each result won.
5. **Bake off embedders locally.** Keep MiniLM as the small fallback. Evaluate at
   least Qwen3-Embedding-0.6B, a compact code-specific model such as CodeRankEmbed,
   and MiniLM on the same Builder retrieval suite, including latency, RAM, disk,
   and cold-start cost. Qwen's [model family](https://arxiv.org/abs/2506.05176)
   supports code retrieval, instruction-aware queries, 32K input, rerankers, and
   matryoshka dimensions. Do not pick it solely from vendor aggregate scores.

### P1: repository intelligence and model control

6. **Add multi-language symbol and dependency maps.** Use incremental parsers for
   definitions/references/imports and persistent LSP or compiler indexes where
   available. Add call/data-flow slices incrementally, beginning with languages in
   the evaluation workload. Give slices exact source spans instead of prose-only
   summaries.
7. **Index Git history as a separate evidence class.** Retrieve commits, diffs,
   messages, linked issue IDs, blame, churn, co-change files, prior tests, and
   reverts. Map historical entities to current symbols and mark unmappable results
   as historical leads. Never let old history satisfy current verification.
8. **Add a phase router and shrink tool schemas.** Classify a turn into answer,
   inspect, localized fix, broad feature/refactor, or verification. Expose only
   valid operations for the durable phase. Simple fixes get search/read/edit/test;
   hypothesis, candidate, and review machinery appears only when evidence or task
   complexity warrants it. This is typed routing, not model-name or prompt-string
   special casing.
9. **Add provider conformance and calibration.** `doctor` should verify one normal
   answer, one native tool call, one parallel batch, JSON mode, maximum output,
   context-overflow reporting, retry behavior, and resume integrity. Evaluate a
   small sampling grid rather than assuming the imported temperature of 0.6 is
   optimal for coding. Persist results by endpoint/model fingerprint.
10. **Improve memory abstention and observability.** Do not return a dense result
    merely because it is top-ranked. Calibrate relevance per embedding fingerprint
    on held-out positives and repository-local negatives, require a margin or
    reranker decision, and include scores/reasons in diagnostics. Track misses,
    false positives, stale hits, and notes never retrieved. Keep task state separate
    from similarity search.

### P2: expensive intelligence, gated by evidence

11. **Run a bounded explorer context.** Let an exploration-only call use parallel
    search/read/graph tools, then return paths, line ranges, hashes, open questions,
    and competing localization hypotheses. Keep its raw traffic outside the solver
    context while retaining it in the journal.
12. **Use test-time scaling for hard tasks.** Generate two to four independent
    plans or candidate patches in disposable copies. Compare them with focused
    tests, static diagnostics, mutation/counterfactual checks, and structured
    summaries. Continue from the most informative point instead of voting on final
    prose.
13. **Learn from successful and failed trajectories.** Store reusable procedures
    only when supported by fresh verification. Mine repeated harness failures into
    candidate routing, tool, or error-message improvements; promote them only after
    held-out regression gains. Training a small explorer, reranker, or memory
    controller on Builder trajectories becomes reasonable after enough clean data
    exists.

## User pain points to design against

These issue reports are examples, not prevalence estimates:

* Indexing can fail while the visible UI appears successful, leaving the model with
  no repository context ([Continue #4648](https://github.com/continuedev/continue/issues/4648)).
  Builder should make degraded retrieval visible and keep exact/FTS search usable.
* Branch and repository scoping is easy to get wrong; Continue documents that one
  FTS index can return results across tags even though its vector index is
  branch-scoped ([indexing design](https://github.com/continuedev/continue/blob/main/core/indexing/README.md)).
  Builder should use one scope rule for every retrieval artifact.
* Local-model compaction can ignore configured output budgets and fail repeatedly
  ([Cline #13127](https://github.com/cline/cline/issues/13127)). Builder's explicit
  budget and retained originals are good; provider conformance and compaction
  evaluation still need strengthening.
* Oversized file reads can create unrecoverable retry loops
  ([Cline #5251](https://github.com/cline/cline/issues/5251)). Builder already
  rejects large reads with bounded range guidance and preserves history.
* Agents repeat plans or enter the wrong workflow instead of editing
  ([Aider #2864](https://github.com/Aider-AI/aider/issues/2864),
  [Cline #13492](https://github.com/cline/cline/issues/13492)). A typed phase router,
  narrower action set, and observed-state progress ledger address this more cleanly
  than model-specific phrases.
* Tool calls can be lost or mismatched across the model/adapter boundary
  ([Cline #11695](https://github.com/cline/cline/issues/11695)). Builder's atomic
  batch validation and journal are strong; `doctor` should exercise the exact live
  endpoint behavior before a long task.

## Release standard

Call a capability production-ready only when it improves repeated held-out task
completion or retrieval metrics at acceptable latency and has deterministic tests
for stale data, cancellation, restart, wrong-worktree isolation, embedding/model
migration, and partial index failure. Report failures and added cost alongside
wins. A vector count, successful embedding call, or isolated benchmark headline is
not a release criterion.
