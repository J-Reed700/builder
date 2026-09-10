# Verification — 9 September 2026

These checks ran on macOS Apple Silicon while implementing remote control. All
model and tool fixtures used disposable workspaces; the existing project and
saved conversations were not used as mutation targets.

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace --locked` | 202 passed, 0 failed; 6 opt-in live tests skipped by default |
| `node tests/remote_web.cjs` | Passed: lost POST acknowledgements and rejected starts retain drafts, with no automatic replay |
| Real local CPU embeddings | Passed: 384 normalized dimensions; vectors persisted and semantic retrieval worked after storage reopen |
| File-based config migration | Passed: exact config copy, private permissions, no overwrite, unchanged session location, and separate remote config reload |
| Existing OpenCode / Continue configs | Both imported with Basic Auth preserved; OpenCode provider sampling retained and live authenticated model discovery passed |
| Configured live chat model | Passed: resumed a compacted fixture, made the expected edit, and verified it with 3 tool calls |
| Configured remote embedding endpoint | Unavailable: HTTP 502 Bad Gateway; no successful live remote-embedding claim |
| Mock remote embeddings | Passed: authentication, vector validation, persistence, deadlines, and lexical fallback |
| Docker-to-host integration | Passed on Docker Desktop: auth, IPv4 routing, browser-style approval, host file creation, durable history, duplicate rejection |
| Browser interface | Two independent chats, draft switching, approval, rename/search, archive/restore, rewind and durable draft recovery, JSON export/copy, reconnect/disconnect, and mobile/desktop layout checked; no console errors |
| Terminal regressions | Paste/resize/menu, interruption/rewind, compaction, memory modes, and pipeline settings passed |

The fifteen remote HTTP regressions cover missing authentication, origin validation,
body limits, workspace scoping, session locks, uncertainty, single-use approvals,
denial/read-only operation, pause during generation and approval, idle-memory
cancellation, original-history pagination, and concurrent duplicate submissions. Added cases verify simultaneous chat/approval
isolation, the four-chat cap, shutdown releasing every lock, archive/restore,
rename/search, stale rewind rejection, saved-turn cancellation, and explicit
remote compaction preserving originals without continuing the task. A schema-5
to schema-6 migration test verifies preserved messages and uncertain tool claims.

The in-app browser did not report a file-download event for the initial automatic
blob download. Export now presents a user-initiated download link and a verified
copyable JSON fallback. Native download completion in other browsers was not
independently verified here.

Configuration follow-up: the default file is now `~/.config/builder/config.toml`;
`XDG_CONFIG_HOME` is supported and explicit `--home` stays self-contained. Added
failure checks for invalid/oversized legacy config and oversized saves preserving
the previous readable file. The README now covers direct file editing and Basic
Auth. The actual OpenCode config exposed missing provider-level sampling support;
`temperature`, `topP`, and `topK` now import with the existing auth settings.

Additional fixes made during verification:

- Export now returns original active messages after compaction, covered by a
  subprocess regression test.
- The agent yields after committing each tool result so an adapter can observe
  cancellation before the next tool/model request, including after a synchronous
  approval callback.
- Docker startup resolves the configured upstream to IPv4, preventing intermittent
  failures when Docker Desktop supplies both IPv4 and IPv6 host-gateway records.
- The terminal paste smoke test now checks the current “Endpoint accepted the
  request” status instead of the removed “Prompt saved” label.

The source includes a release packaging workflow for Linux x86-64, macOS Apple
Silicon, and Windows x86-64. Only the local macOS build and Docker Desktop path
were exercised in this environment; the other platforms require their CI runs.
The user's public DNS, HTTPS proxy, Pangolin, and WireGuard deployment were not
changed or exercised. Follow [the remote setup guide](REMOTE_CONTROL.md) for that
last deployment step.

These are concrete software checks and small live-model fixtures, not proof that
all endpoints are available or that a model will complete every coding task.
