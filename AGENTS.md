# Builder engineering conventions

- Keep the crate dependency graph acyclic: application -> provider/tools -> core.
- Preserve the persistence and recovery invariants in ARCHITECTURE.md.
- Use enums for closed domain states and RAII for resource lifetimes.
- Keep provider details out of the agent and terminal details out of library crates.
- Never dispatch provisional tool calls or automatically replay uncertain execution.
- Never silently truncate conversation history. Changes to context selection must be explicit and preserve archived originals.
- Keep output, file reads, retries, and subprocess lifetimes bounded.
- Add a behavioral failure test when changing reliability or permission semantics.
- Use `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --locked` before delivery.
- Prefer small concrete types to speculative abstractions. Do not introduce unsafe code.
