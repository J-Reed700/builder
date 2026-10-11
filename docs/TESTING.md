# Testing Builder

Builder is tested at four boundaries: Rust domain/runtime behavior, the browser
adapter in Jest, real browser journeys against the Rust host, and real terminal
sessions. Routine tests use disposable workspaces and scripted local model
endpoints. They do not require model credentials or download embedding models.

## Run the suite

Install Rust 1.95+, Node.js 22+, and Python 3.11+. From the repository root:

```sh
npm ci
npx playwright install chromium firefox webkit
npm run test:all
```

On Linux, use `npx playwright install --with-deps chromium firefox webkit` to
install the browser system libraries as well. Dependencies and browser engines
need a network connection during setup; the deterministic tests run locally.

`test:all` runs formatting, Clippy, workspace Rust tests, Jest with coverage,
coverage-gate regressions, retrieval evaluation, terminal tests, and all browser
projects. It fails on the first failed command. Unix PTY tests run on Linux and
macOS; Windows prints an explicit platform exclusion. The Docker gateway test is
opt-in locally and remains mandatory in Linux CI:

```sh
python3 scripts/test.py --docker
python3 scripts/test.py --skip-browsers
```

Useful focused commands:

```sh
cargo test --workspace --locked
cargo test --locked --test remote
cargo test --locked --test input_properties
cargo test --locked -p builder-provider --test sse_boundaries
npm test
npm run test:unit:watch
npm run test:browser -- --project=chromium
npm run test:browser:ui
```

## What each layer verifies

| Layer | Scope | Main cases |
| --- | --- | --- |
| Rust unit and integration | `src/`, `crates/`, `tests/*.rs` | Persistence, tool claims, crash recovery, stale observations, permissions, provider retries, streaming, budgets, research, memory, code retrieval, scheduling, concurrency, compaction, and protocol boundaries |
| Jest + jsdom | `tests/web/` | Production HTML and JavaScript; authentication, gateway setup, drafts, busy/archive states, approval decisions, pagination, bounded exports, queued sends, stale responses, clipboard failures, focus, and error recovery |
| Playwright + actual Builder process | `tests/browser/` | Login/reload/disconnect, durable Unicode history, actual approved/denied writes, read-only policy, lost acknowledgements, duplicate input, pause/retry/cancel, provider failure, compaction queues, chat management, exports, scoped folders, hostile HTML, and viewport/focus behavior |
| Unix PTY | `tests/terminal_*.py` | Large paste, resize, interruption, rewind, compaction, pipeline settings, and memory menus in raw and plain terminals |
| Retrieval gates | `eval/retrieval/`, `tests/retrieval_eval_cli.py` | Held-out query quality, manifest validity, and nonzero exit status when quality falls below the gate |
| Docker gateway | `tests/remote_container.py` | Pairing, outbound WebSocket relay, identity/authentication, browser approval, host mutation, durable history, and duplicate-request rejection |

Playwright runs every journey in desktop Chromium, Firefox, and WebKit, plus
Pixel and iPhone browser configurations. These are browser/device emulations,
not native phone or hardware tests. Every test starts a separate Builder process,
temporary home/database/workspace, and a scripted model HTTP server. The browser
loads the actual host's HTML and API. Fault tests deliberately interrupt selected
network responses; they still verify the real saved history. Uncaught browser
JavaScript errors fail the test. No automatic retry hides an intermittent failure.

The Jest harness loads the actual HTML and requires `remote/web/app.js`, so Jest
instruments production code. It replaces HTTP, clipboard, and missing jsdom
capabilities; it does not maintain a second implementation of the app. Tests
operate through DOM events and observable UI state. Timers are controlled, and
out-of-order response cases use deferred promises rather than wall-clock sleeps.

The new Rust property tests exhaust every two- and three-part partition of a
Unicode SSE fixture and compare 32,768 deterministic composer operations against
an independent reference model. Boundary tests check UTF-8 byte limits, full
rejection without partial mutation, bounded undo, malformed API requests, and
multiline SSE size accounting.

## Coverage and reports

JavaScript gates apply to every file under `remote/web/`, including files never
imported by a test:

| Metric | Required |
| --- | ---: |
| Lines | 100% |
| Functions | 100% |
| Statements | 99% |
| Branches | 94% |

`npm test` writes HTML, LCOV, and JSON under `coverage/web/`. Browser failures
retain screenshots, video, traces, and host logs. Open `playwright-report/index.html`
or run `npx playwright show-report`; a trace can be opened with
`npx playwright show-trace path/to/trace.zip`.

To measure Rust:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked --version 0.9.1
npm run test:coverage:rust
```

The coverage runner instruments and measures **all workspace crates**, writes
`coverage/rust/coverage.json`, `coverage/rust/lcov.info`, and
`coverage/rust/html/index.html`, then checks the per-crate floors in
`tests/coverage-baseline.json`. Missing data and newly added crates without an
explicit floor fail the gate. The workspace floor is 76% lines; core and provider
floors are 90%. These are measured starting floors, not a claim of full Rust
coverage. Raise them as coverage improves; do not lower them to accommodate an
unexplained regression. Stable LLVM coverage here measures lines, functions and
regions; it does not measure Rust branch coverage.

For Rust coverage that also includes terminal, retrieval and browser execution
on Linux or macOS:

```sh
npm run test:coverage:all
```

This additionally builds the instrumented CLI and runs all Unix PTY scenarios,
retrieval evaluation, and Chromium journeys against it. It takes longer and
requires the same browsers/Python prerequisites as `test:all`. Keep ordinary and
extended coverage numbers distinct when comparing runs. The extended run also
requires 83% workspace line coverage and verifies that terminal and browser
adapter coverage was actually recorded. Linux CI runs this extended gate.
Generated reports are
ignored by Git; CI uploads reports and browser diagnostics for 14 days.

## Coverage limits and remaining work

100% line coverage does not mean every possible behavior has been tested. The
browser branch floor leaves explicit room for defensive/unusual paths. The Rust
report still has gaps, particularly optional embedding initialization, some CLI
and terminal adapters, OS service integration, and gateway disconnect/recovery
paths. Use the HTML report to select the next observable failure case.

Live-model and installed-local-model evaluations remain explicitly ignored in
the default Rust suite. Run them only with the endpoints/assets described in
[LLM evaluations](LLM_EVALUATIONS.md), [Memory](MEMORY.md), and
[Research runtime](RESEARCH_RUNTIME.md). Scripted responses verify runtime
contracts; they do not establish model quality. Production reverse proxies,
real mobile keyboards, OS-specific services, and power-loss behavior also need
environment-specific validation.

## Adding a regression

1. Reproduce the failure at the smallest useful boundary. Assert preserved
   history, exact output, denied effects, request counts, or visible state.
2. For user workflows, add a real browser or PTY journey in addition to the
   focused unit/integration case. Use isolated state and condition-based waits.
3. For failures during writes, prove that no automatic replay occurs. Include
   a late response after disconnect or a concurrent operation when relevant.
4. Run the focused suite, then the affected layers and coverage gates. Do not
   exclude production files, weaken assertions, or add retries to make a test pass.

Framework reference: [Jest coverage configuration](https://jestjs.io/docs/configuration#coveragethreshold-object),
[Playwright test documentation](https://playwright.dev/docs/intro), and
[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov).
