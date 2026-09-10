# Retrieval evaluation

Run `cargo run --locked --example retrieval_eval -- /absolute/path/cases.json`.
The runner uses temporary databases, reads each supplied checkout, and writes one
JSON report per case and variant to stdout. It does not modify the checkout or
contact an embedding/model endpoint. Set `BUILDER_LOCAL_MODEL_DIR` to an
already installed pinned MiniLM directory to also evaluate local dense retrieval.
That variant requires complete vector coverage, has a 30-minute build deadline,
and reports embedding build time separately; it never downloads model assets. Keep output outside the evaluated repository
so it cannot become a retrieval candidate.

A manifest is a JSON array of objects with:

- `id`: unique case ID.
- `workspace`: absolute path, or a path relative to the manifest, to an independently prepared checkout.
- `query`: issue description without the solution or target filename hints.
- `relevant`: objects containing `path` and inclusive `lines: [start, end]`.
- `source_hashes`: mapping of labeled paths to their SHA-256 file digests.

Changed labeled source causes an error rather than silently reusing stale labels.
For comparisons, keep the entire checkout fixed at the same pre-fix revision,
including untracked files; labeled hashes alone do not pin distractor files.
Choose line labels through independent review of the eventual fix and supporting
code. Do not tune and evaluate on the same cases.

The initial variants compare exact/lexical retrieval with zero versus two syntax
reference graph hops. History is disabled. Dense retrieval is disabled in those two variants to
isolate the comparison; the optional third variant enables local embeddings. These are experimental variants, not changes to user configuration.
Reports contain file recall, unique-file reciprocal rank and binary NDCG, unioned
line recall and precision, selected line count, elapsed time including capture,
and the original ranked evidence. Duplicate or overlapping chunks do not increase
coverage. Selected-span metrics and complete-excerpt-line metrics are reported
separately. These metrics do not measure successful edits.

Next: a reviewed multi-repository corpus, alternate local embedding models, repeated paired trials, and downstream edit/test outcomes.
No embedding or reranking default should change based on the smoke fixtures alone.

## Diagnosing a miss

Reports include `candidate_trace`: all fused candidates before per-file diversity
and result limits, with source ranges and separate lexical, semantic, exact and
graph ranks. This developer trace is not added to the coding model's context.
Compare the gold ranges with this trace and `evidence.results`: a candidate
present only in the trace lost during selection; absent candidates need an
index/candidate-generation investigation. Inspect excerpts separately because
selected ranges can be larger than the excerpt budget.

The September 10 smoke investigation found two extraction/ranking failures:
leading documentation was assigned to a preceding declaration and local types
split callable bodies; exact declaration/reference matches and repeated graph
visits could give the same passage multiple votes. A separate metadata bonus
also counted exact matches again. BM25 also overvalued repeated path/reference
metadata; their column weights are now 0.1 versus 1 for declarations and content.
An ablation restoring equal column weights lost the reasoning range in the
lexical-only case. Callable chunks now retain adjacent leading
comments/attributes and local types. Fusion gives each retrieval channel one
vote per chunk and retains metadata hit counts only for diagnostics. A snapshot
format version forces re-extraction even when source files have not changed.

These cases are now development regressions, not held-out evidence of general
quality. Embedding model selection remains unchanged.

### Smoke result after these fixes

On the same frozen seven-file source subset (119 chunks after re-extraction),
all three development queries achieved selected-range line recall 1.0 in all
three variants: lexical, lexical plus graph, and lexical plus graph plus local
MiniLM. Previously the reasoning and offline-embedding cases had line recall 0.
All three dense runs had complete coverage (119/119 vectors). No model or query
phrase-specific routing was added.

This does not mean every answer passage ranks first. Unique-file reciprocal
ranks for reasoning were 1/3, 1/3 and 1/2, respectively; for compaction they were
1, 1 and 1/3. Dense retrieval therefore worsened file ordering in one case even
though the labeled range remained selected. A reviewed held-out corpus and
actual edit/test outcomes are still required before claiming general gains.

## Regression gates and the checked-in suite

Run the deterministic development corpus:

```sh
cargo run --locked --example retrieval_eval -- eval/retrieval/cases.json
cargo test --locked --test retrieval_regressions --example retrieval_eval
python3 tests/retrieval_eval_cli.py target/debug/examples/retrieval_eval
```

CI runs both the corpus and manifest validation alongside workspace tests. The
eight corpus cases cover Rust, Python, TypeScript and Go; exact symbols, natural
language descriptions, competing metadata, required evidence across files and expected abstention. Fixtures and
all distractor files have pinned hashes. These are synthetic development cases,
not an independent benchmark or proof of coding ability.

`expectation` is optional. Its default requires full file and selected-line
recall. Each retrieval case can specify explicit gates:

```json
"expectation": {
  "kind": "retrieve",
  "thresholds": {
    "min_file_recall": 1.0,
    "min_line_recall": 1.0,
    "min_passage_reciprocal_rank": 0.2,
    "min_excerpt_line_recall": 1.0,
    "max_results": 10
  }
}
```

Use `"expectation": {"kind": "abstain"}` with an empty `relevant` array for
negative cases. These fail when any code result is returned. Thresholds must be
finite and in [0,1]; max_results must be 1–20. Unspecified passage/excerpt minima
are zero; excerpt coverage becomes a gate only when explicitly requested.

The report adds `assessment` with pass/fail reasons, candidate coverage, selected
coverage, complete excerpt coverage, and scores at result cutoffs 1, 3, 5 and 10.
Passage reciprocal rank uses actual result positions and requires overlapping
labeled lines, so returning a wrong region in the right file cannot earn credit.
A clipped partial line earns no excerpt credit. Duplicate/overlapping results
cannot inflate coverage. Coverage arithmetic rejects overflow.

Every case/variant must pass its gates. Quality failures retain all reports and
produce a nonzero exit after the suite finishes; malformed input, stale hashes,
out-of-file labels or incomplete required semantic coverage stop immediately.
There is no aggregate average that can hide an individual regression.

Behavioral tests also cover result-limit configuration, disabling the index,
workspace isolation, contract edits, rename/deletion, reopening the database,
long Unicode callable tails, clipping, duplicate coverage, invalid gates and
inconsistent abstention reports. The tests mutate only temporary workspaces.
Local semantic coverage remains an optional run with already installed assets;
CI does not download models or claim to test dense retrieval without them.
