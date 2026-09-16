# Contributing

Issues and small, focused pull requests are welcome. Before making a larger
change, open an issue so the design and compatibility impact can be discussed.

## Engineering guidelines

- Keep the crate dependency graph acyclic: application → provider/tools → core.
- Preserve the persistence and recovery invariants in
  [ARCHITECTURE.md](ARCHITECTURE.md).
- Keep provider details out of the agent and terminal details out of library
  crates.
- Use enums for closed domain states and RAII for resource lifetimes.
- Bound output, file reads, retries, and subprocess lifetimes.
- Never dispatch provisional tool calls, replay uncertain execution, or silently
  truncate conversation history.
- Add a behavioral failure test when changing reliability or permission
  semantics.
- Prefer small concrete types and do not introduce unsafe code.

## Checks

Run these before opening a pull request:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Changes to repository retrieval should also run:

```sh
cargo run --locked --example retrieval_eval -- eval/retrieval/cases.json
```
