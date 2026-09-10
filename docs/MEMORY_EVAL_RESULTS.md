# Memory evaluation — September 8, 2026

The memory implementation passed deterministic storage, runtime, HTTP, permission and recovery checks. Live model behavior remains imperfect. These are development measurements, not a release claim that the original Arena investigation is fixed.

## Paired model trial

Both runs used the same compiled test executable (SHA-256 `318a366def8190a67710b9c89ae9dfcaf1ce1e0b93df1539eca43772dcee93b2`), Qwen3.8-27B-Q5_K_S.gguf, completion settings, small fixtures, graders, and limits. Memory was the changed runtime setting. One trial per case per mode is insufficient for statistical conclusions. The executable included the initial concurrently developed research tool; subsequent research changes are not represented by this pair.

| Case | Memory off | Memory on | Off seconds | On seconds |
|---|---|---|---:|---:|
| Resume findings | Fail: answer contract | Pass | 38.5 | 77.9 |
| Stale handoff | Fail: answer contract | Pass | 40.7 | 57.7 |
| User correction | Pass | Pass | 14.2 | 50.6 |
| Unknown value, read only | Fail: answer contract | Fail: answer contract | 18.3 | 44.1 |
| Total | **1/4** | **3/4** | **111.7** | **230.2** |

All file-content checks passed in both modes. “Answer contract” means the final answer did not parse as exactly the requested JSON facts. It can represent wrong facts, added fields, or formatting; the initial pair did not retain answers, so it does not distinguish those causes. The grader was not relaxed. Subsequent reports retain bounded answer excerpts for diagnosis.

Memory off used 19 chat requests and 121,141 serialized prompt bytes; memory on used 26 and 200,139. Counts include compaction and memory extraction, not separate embedding HTTP requests. Elapsed time includes embedding work. Neither mode repeated an identical source read during the graded continuation. The seed deliberately supplies repeated historical reads to trigger recovery. These are cold-memory tests with extraction from source evidence, not a warm cross-session retrieval benchmark. They do not prove reduced re-reading on large repository tasks.

A separate diagnostic rerun of the missing-value case on the later workspace build passed, returning `{"shield_duration_seconds": null}` in 49.1 seconds. That is an additional trial, **not a replacement for the recorded failure**. The older pre-memory baseline also passed all four once, reinforcing the need for repeated held-out evaluations instead of attributing every failure to one component.

Reports:

- [Memory off](llm-eval-memory-off-2026-09-08.json)
- [Memory on](llm-eval-memory-on-2026-09-08.json)
- [Diagnostic rerun](llm-eval-memory-diagnostic-2026-09-08.json)
- [Earlier baseline](llm-eval-baseline-2026-09-08.json)

## Embeddings

The configured Nomic embedding endpoint returned **HTTP 502 Bad Gateway** during the live smoke test. Semantic retrieval against that server was therefore not verified. The runtime retained notes and fell back to FTS/keyword retrieval. This makes the live pair a memory-on fallback measurement, not evidence of a semantic-search improvement.

Controlled HTTP integration tests passed semantic ranking without keyword overlap, persistence/restart, revision namespaces, endpoint-failure fallback, and recovery of pending indexing. Authentication, invalid vectors, multiple results, oversized input, and error-body secrecy were also tested. The live opt-in probe remains available in `tests/memory_runtime.rs`; it correctly fails when the configured server cannot supply vectors.

## Interpretation

The implementation now provides the architectural mechanisms that were missing: bounded reusable findings, source evidence, revisions, stale invalidation, automatic selection, and resumable indexing. It does not establish that all model errors were caused by architecture, or that all architecture problems are solved. The live suite caught failures despite earlier passing trials. Larger tasks, repeated trials, and a healthy embedding endpoint are needed to measure reliability and justify extraction cost.

## Local embedding follow-up

The CLI now defaults to local MiniLM CPU generation, with a pinned one-time model download and local SQLite vectors. The remote profile is no longer selected implicitly. On the user's Mac, the installed release indexed both existing drift-temp notes and returned hybrid retrieval with no embedding error while HTTP proxies pointed to an unreachable local address. The measured fresh-process search took approximately one second; this is a smoke measurement, not a benchmark.

Validation: 136 ordinary workspace tests, strict Clippy and formatting passed. An explicit installed-model test verified 384 normalized dimensions and semantic retrieval after database reopen. Real terminal tests covered raw/plain memory menus, cached offline model setup, preservation of concurrent profile edits and rejection of conflicting memory edits. The previous remote-endpoint failure above is historical and is not a dependency of local mode. See [current memory setup](MEMORY.md).
