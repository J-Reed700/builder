# Rust architecture audit

Reviewed on 2026-10-06 against the existing seven-package workspace, starting at
commit `31caced`. The review covered crate dependencies, domain and persistence
boundaries, agent orchestration, retrieval and memory, provider and tool
contracts, CLI and terminal adapters, remote control, gateway lifecycle,
configuration, and existing regression coverage.

The existing design has a sound foundation: a generic provider contract,
exhaustive typed actions and outcomes, bounded I/O, explicit recovery rules, and
durable conversation ownership. The main organizational weakness was that those
boundaries were clearer between crates than within modules. This change tightens
the internal boundaries and fixes one reproduced cancellation defect. It does not
establish that every execution path is defect-free.

## Findings and disposition

| Priority | Finding | Resolution |
| --- | --- | --- |
| P2 — reliability | Dropping an in-flight gateway proxy future left its request in the bounded pending registry. Repeated cancellations could consume all slots if no host response or disconnect removed them. | A `PendingResponse` drop guard now owns the slot. The regression drops responses after dispatch more times than the registry capacity, checks that capacity returns, and checks that cancellation never enqueues a replay. It failed before the fix. |
| P2 — maintainability | Memory, research, code-index, and todo domain modules implemented repository operations; `Store::conn` was visible throughout core. Schema, connection, session, transcript, tool, and recovery logic shared a 1,024-line store file. | SQL and schema ownership moved under `store/`. Connections and transaction creation are private to that tree. Domain outcomes live in `execution.rs`; old store type paths remain re-exports. Storage methods and schema versions are unchanged. |
| P2 — coupling | Code-index retrieval and maintenance accepted the entire memory runtime solely to use embeddings. Memory policy owned backend selection, network failure state, caching, and vector batches. | A concrete `EmbeddingRuntime` owns those capabilities. Both consumers share it; code-index APIs now accept that narrower type. Memory configuration still determines whether the application supplies it. |
| P3 — organization | The agent's event contract, round loop, request assembly, and tests shared one file, while compaction, instruction memory, and delegation lived at the source root. | The `agent/` facade owns these capabilities. Request construction and context budgeting are separate from round orchestration. Existing behavior and public entry points are retained. |
| P3 — organization | Memory and research mixed evidence validation, retrieval projections, model analysis, and operation policy. Memory tool schemas duplicated the common schema builder in the application. | Feature directories now separate those responsibilities. All tool schemas, including memory, live in `builder-tools`. |
| P3 — adapter coupling | Remote progress formatting called into terminal UI. Gateway pairing, socket handling, and proxying shared its library root; outbound connection code mixed credential persistence with transport. | Shared plain descriptions live in `presentation.rs`. Gateway route groups and outbound credential storage have private modules. Terminal help and streaming rendering are separate behind the existing UI facade. |
| P3 — regression prevention | Dependency direction relied on convention; package metadata and minimum Rust versions were inconsistent across manifests. | Rust syntax and manifest tests enforce selected boundaries. Package version, edition, license, and minimum Rust version are inherited from the workspace. |

## SOLID and Rust design assessment

- **Single responsibility:** feature facades expose operations while private
  modules own evidence policy, SQL, network transport, execution, or rendering.
  A module with a long exhaustive match or a large settings table is not split
  solely to satisfy an arbitrary line-count rule.
- **Open/closed:** provider substitution uses the existing `Provider` trait.
  Tools and wire messages remain closed enums so permission, replay, and
  serialization decisions are checked exhaustively. A plugin registry would add
  complexity without a current requirement.
- **Substitution:** existing fake-provider and local HTTP tests exercise
  complete-response, interruption, retry, and tool-call behavior. Any additional
  production provider must meet those same contracts; trait implementation alone
  is not evidence of behavioral equivalence.
- **Interface segregation:** code retrieval receives an embedding service;
  adapters receive runtime events; the gateway receives relay messages rather
  than workspace or agent state.
- **Dependency inversion:** orchestration depends on the provider contract and
  typed tools. The SQLite repository stays concrete because there is one storage
  backend. Introducing an interface for every struct would obscure ownership
  without providing a useful substitution point.
- **Rust ownership:** session and index locks, subprocess groups, terminal state,
  and now pending proxy responses use lifetime-bound guards. Unsafe code remains
  forbidden by workspace lint configuration.

## Compatibility

CLI commands, configuration formats, database schemas, message ordering, retry
budgets, tool permissions, memory thresholds, and remote wire formats are
preserved. The `builder::subagent`, `builder::remote_connect`,
`builder::memory::definitions`, and `builder_core::store` outcome paths remain
available through facades or re-exports.

One deliberate Rust library signature change narrows code-index `search`,
`search_with_trace`, `maintain`, and `coverage` arguments from
`Option<&MemoryRuntime>` to `Option<&EmbeddingRuntime>`. Callers with a memory
runtime pass `memory.as_ref().map(MemoryRuntime::embeddings)`; callers using
lexical-only retrieval still pass `None`. Repository callers and the retrieval
evaluation example are updated. No dependency versions were upgraded; `syn`,
already present in the lockfile, is now also an explicit test dependency for
syntax-based architecture checks.

## Remaining tradeoffs

1. The application package still contains both runtime services and adapters.
   Visibility and regression checks enforce important boundaries, but separate
   crates would provide stronger compilation and dependency isolation. Extract a
   runtime crate when another binary or consumer actually needs independent
   packaging; doing it now would expand the public API and build configuration.
2. SQLite and bounded filesystem operations remain synchronous. The idle worker
   owns background maintenance, but some foreground calls can still delay async
   cancellation. Profile real latency before introducing worker pools; preserve
   transaction and uncertain-execution semantics if that changes.
3. `Store` remains a broad concrete facade and session-lock ownership remains a
   caller obligation. A scoped session handle could enforce more of that contract
   if independent embedders are added. Current composition and recovery tests
   remain the guardrails.
4. The local embedding implementation is a separate crate but is linked into
   normal application builds. A build without local inference could benefit from
   feature gating; that requires separate packaging and feature-matrix testing.
5. Some orchestration matches and protocol/configuration tables are still large.
   Their size is not itself a design defect. Further splits should identify a
   coherent policy or lifecycle to extract, with behavioral coverage.

## Validation

The baseline workspace tests passed before changes. Final local checks on macOS:

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed |
| `cargo test --workspace --locked` | 379 passed, 0 failed, 10 opt-in tests ignored |
| `cargo run --locked --example retrieval_eval -- eval/retrieval/cases.json` | All 16 case/variant quality gates passed |
| `cargo test --locked --example retrieval_eval` | Manifest regression passed |
| `python3 tests/retrieval_eval_cli.py target/debug/examples/retrieval_eval` | Failure exit, report retention, and label checks passed |
| `python3 tests/terminal_smoke.py target/debug/builder` | Paste, input, resize, and terminal cleanup passed |
| `python3 tests/terminal_interrupt.py target/debug/builder` | Interruption, rewind, cancellation, and recovery passed |
| `python3 tests/terminal_compact.py target/debug/builder` | Compaction and checkpoint recovery passed |
| `node tests/remote_web.cjs` | Lost acknowledgements and queued-message recovery passed |
| `git diff --check` | Passed |

The ignored tests require explicitly configured live model endpoints, installed
local models, or replay fixtures. They were not enabled and no models were
downloaded. Docker deployment and Windows/Linux execution were not validated
locally; the existing CI workflows cover those environments. The reproduced
gateway cancellation test was observed failing before the guard was added and
passing in the final workspace suite.
