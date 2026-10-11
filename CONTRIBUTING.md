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
- Keep domain values free of SQL and place repository implementations under
  `builder-core/src/store/`. Database connections remain private there.
- Give consumers the capability they need: code-index operations take the shared
  embedding service, not the entire memory runtime. Keep runtime policy free of
  terminal and HTTP adapters.
- Inherit package version, edition, license, and Rust version from the workspace.
  Architecture boundary tests run with the normal workspace test suite; update
  the documented dependency rule deliberately when introducing a new crate.

## Checks

Run these before opening a pull request:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm ci
npm test
npm run test:browser
```

Install browser engines once with `npx playwright install chromium firefox webkit`.
`npm run test:all` also runs retrieval and real-terminal scenarios. See
[Testing](docs/TESTING.md) for coverage gates, failure artifacts, optional live
evaluations, and commands for each test layer.

Changes to repository retrieval should also run:

```sh
cargo run --locked --example retrieval_eval -- eval/retrieval/cases.json
```
