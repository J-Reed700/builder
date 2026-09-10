<div align="center">

# ▰ Builder

**A coding agent on your computer. A conversation you can pick up anywhere.**

Connect your model server, work in your terminal, or use a self-hosted browser interface.
Your workspace, conversations, and memory stay on the host running Builder.

[Get started](#get-started) · [Remote control](#remote-control) · [Memory](#persistent-memory) · [Documentation](#documentation)

</div>

Builder is a Rust coding agent for OpenAI-compatible model servers. It can inspect a repository, edit files, run commands with your approval, and resume saved work. It supports local CPU embeddings, persistent memory, and automatic context compaction with the original transcript retained.

**Version 0.1:** usable software with extensive failure tests; model quality and endpoint compatibility still depend on your server. Builder supplies the agent, not a hosted chat model or a sandbox.

## Get started

You need an OpenAI-compatible chat endpoint and its model ID. Examples include a local Ollama, LM Studio, llama.cpp, or vLLM server, or a hosted compatible endpoint. Chat and tool-call support depend on the server and chosen model.

### 1. Install

**From source**, install [stable Rust](https://rustup.rs), then run:

```sh
git clone https://github.com/J-Reed700/builder.git
cd builder
./install.sh
```

If you downloaded the source ZIP instead, extract it and run `./install.sh` from that folder.

This runs `cargo install --path . --locked`. The first build downloads dependencies and can take several minutes. Windows users can run that Cargo command directly. The code uses Rust 2024; Rust 1.95 is the currently tested toolchain. Linux source builds need a C/C++ compiler and the usual build tools because SQLite and the embedding runtime include native components.

**From a packaged binary**, extract the archive and run `./install.sh` to copy `builder` into `~/.local/bin`; no Rust or sudo is needed. Add that directory to `PATH` if the installer asks. On Windows, put `builder.exe` in a folder on `PATH`. The [release workflow](.github/workflows/release.yml) builds Linux x86-64, macOS Apple Silicon, and Windows x86-64 archives with checksum files. Packaged binaries are produced as downloadable artifacts when the [Release binaries workflow](https://github.com/J-Reed700/builder/actions/workflows/release.yml) runs. No tagged release binaries have been published yet.

### 2. Edit your config file

Builder reads **`~/.config/builder/config.toml`**. Create that directory and file in your editor, or copy [the example config](examples/config.toml). Configuration commands are optional shortcuts for editing the same file.

```toml
default_profile = "local"

[profiles.local]
base_url = "http://localhost:11434/v1"
model = "qwen3:8b"
```

Replace the URL and model ID with those from your server, then run `builder doctor`. It checks configuration, storage, model discovery, ordinary generation, JSON-object output, one native tool call, and a parallel tool-call batch; results are saved by endpoint/model fingerprint. `builder models` only lists the advertised model IDs.

`XDG_CONFIG_HOME` changes the config root: `$XDG_CONFIG_HOME/builder/config.toml`. On Windows the default is `%USERPROFILE%\.config\builder\config.toml`. `builder config init` creates a default file if needed and prints its exact path. `--home DIR` / `BUILDER_HOME` keeps both config and data under that directory for a portable installation.

**Upgrading:** Builder copies an existing config from its old application-data directory into the new location on first launch, without replacing an existing new config. The original file stays in place. Chats, memory, model downloads, session locks, and the remote token stay in their existing data directory; this change does not move or reset them. Edit the new config from then on, and restart existing Builder processes after upgrading.

### Custom endpoint with HTTP Basic Auth

For a model server behind Pangolin or another gateway using Basic Auth, put this in `~/.config/builder/config.toml`:

```toml
default_profile = "remote"

[profiles.remote]
base_url = "https://models.example.com/v1"
model = "your-exact-model-id"

[profiles.remote.headers]
Authorization = { env = "BUILDER_BASIC_AUTH" }
```

The environment variable must contain the **complete header**, `Basic <base64(username:password)>`. This command prompts for your credentials without putting the password in shell history (requires Python 3):

```sh
export BUILDER_BASIC_AUTH="$(python3 -c 'import base64,getpass; u=getpass.getpass("Basic Auth username: "); p=getpass.getpass("Basic Auth password: "); print("Basic " + base64.b64encode((u+":"+p).encode()).decode())')"
builder doctor
builder
```

Alternatively, set `Authorization = "Basic YOUR_BASE64_VALUE"` directly in the file. Base64 is not encryption; a literal value is a stored credential. Use HTTPS for remote endpoints, and keep this file private (`chmod 600 ~/.config/builder/config.toml` on macOS/Linux). Builder creates and migrates its config with owner-only permissions.

**Basic Auth goes in `headers.Authorization`, not `api_key`.** Builder sends that header for model discovery, chat, retries, and embeddings. It takes precedence over bearer auth. An embedding model in another profile needs its own `headers` section. For an API-key endpoint instead, put `api_key = { env = "MODEL_API_KEY" }` inside the profile and omit the Basic Auth header.

Start `builder remote` or `builder remote connect` from a shell or service that has `BUILDER_BASIC_AUTH` set. Model credentials belong to the host process and never enter Builder Gateway. Pangolin SSO is a separate browser login. [Remote setup →](docs/REMOTE_CONTROL.md)

### Already configured OpenCode or Continue?

Import your existing endpoint, model settings, and Basic Auth headers; the source file is left untouched:

```sh
# Choose the tool whose model config you want to import:
builder config import ~/.config/opencode/opencode.json --format opencode --activate
# Or:
builder config import ~/.continue/config.yaml --format continue --activate
```

Builder supports OpenCode custom OpenAI-compatible providers and Continue `provider: openai` models. Imports preserve supported custom headers, including `Authorization: Basic …`, and write Builder's normal TOML file. [Supported fields and import limits →](docs/CLI_GUIDE.md#import-continue-or-opencode-models)

### 3. Start building

```sh
cd /path/to/your/project
builder
```

Try: **“Explain how this project is organized, then suggest one small improvement.”**

Builder reads automatically and asks before changing files or running shell commands. Use `/settings` to configure the runtime, `/memory` to enable persistent memory, and `/help` to browse commands.

## Everyday use

| What you want | Command |
| --- | --- |
| Start in a particular project | `builder -C /path/to/project` |
| Run one task | `builder run "Explain this repository"` |
| List saved conversations | `builder sessions` |
| Reopen a conversation | `builder resume SESSION_ID` |
| Send a follow-up from a script | `builder run --session SESSION_ID "Now add tests"` |
| Export the original active transcript | `builder export SESSION_ID --json` |
| Prevent file changes and shell commands | `builder --approval read-only` |
| Automatically approve tool actions | `builder --auto` |

Inside chat, **Ctrl+C** pauses the current run. Send a follow-up to change direction or use `/retry` to continue the saved turn. `/rewind` archives the last turn and restores its prompt; it does not undo file changes. Reopening an interactive session waits for your instruction.

For multiline input, use **Alt+Enter / Ctrl+J**. Large pastes fold into compact blocks without losing their contents. On macOS, **Ctrl+V** reads the clipboard directly. [Terminal and command guide →](docs/CLI_GUIDE.md)

## Persistent memory

Conversations are saved automatically. Memory is a separate, optional store of short findings backed by source reads, plus explicit user preferences.

```sh
builder memory enable --local
builder memory status
builder memory remember response-style "Keep explanations concise and include test results."
builder memory search "how configuration is loaded"
```

Local setup downloads about **91 MB** once, verifies the pinned files, and checks that inference works. After that, the CPU model generates **384-dimensional embeddings on-device**. Notes and vectors live in SQLite. Use `builder memory enable --lexical` to skip the model download, or explicitly choose a remote embedding profile.

Relevant notes carry source evidence. Changed or rewound sources invalidate old findings. Memory can survive compaction and help another conversation in the same checkout; it never grants tool permissions. Automatic foreground recall uses fast keyword lookup, while explicit search combines keyword and semantic rankings. [Memory setup, retention, and limits →](docs/MEMORY.md)

## Repository intelligence

While a chat is idle, Builder indexes the current checkout into source-hashed,
structural chunks. Exact identifiers and paths, FTS5/BM25, reference expansion,
and optional local vectors feed the `code_search` tool. Filesystem events prompt
debounced refreshes, periodic complete scans recover missed events, and every
selected result is checked against the current file hash before it reaches the
model. Failed scans keep the last complete generation, while lexical retrieval
continues if embeddings are unavailable. Configure the watcher, refresh rate,
embedding batches, retrieval limits, and all index bounds from `/settings`; inspect
generation and semantic coverage with `/status`.

[Index lifecycle, ranking, freshness, and settings →](docs/CODE_INDEX.md)

## Remote control

Deploy Builder Gateway beside Pangolin/Newt, then connect Builder from the project you want to control. Builder makes an outbound encrypted connection, so the computer needs no public listener, Docker bridge address, or inbound firewall rule. Code, model credentials, SQLite, embeddings, and tool execution remain on that computer.

```text
Your browser
    │ HTTPS
    ▼
Pangolin / your reverse proxy
    │ private network
    ▼
Docker: Builder Gateway
    ▲
    │ outbound authenticated WebSocket
    │
Builder on your computer → workspace + model endpoint + SQLite
```

### Gateway behind Pangolin

If Pangolin/Newt already has a Compose project, merge the image-only gateway files into that project. It joins the existing default network, so Newt can reach `builder-gateway:8080` by service name:

```sh
# Put BUILDER_GATEWAY_ORIGIN and BUILDER_DOMAIN in the existing stack's .env.
docker compose -f compose.yml \
  -f /path/to/builder/remote/compose.yaml \
  -f /path/to/builder/remote/pangolin-labels.yaml \
  up -d builder-gateway
```

This pulls `ghcr.io/j-reed700/builder-gateway:latest`; it does not build Builder or mount its source. You can instead copy the `builder-gateway` service and `builder-gateway-data` volume from those two small files directly into an existing Compose file.

For a standalone gateway stack:

```sh
cp remote/.env.example remote/.env
# Set BUILDER_GATEWAY_ORIGIN and BUILDER_DOMAIN in remote/.env, then:
cd remote
docker compose -f compose.yaml -f pangolin-labels.yaml up -d
```

Open the public URL, sign in through Pangolin, and click **Generate connect command**. Run the command in your project directory:

```sh
cd /path/to/project
builder remote connect https://builder.example.com --code CODE_FROM_DASHBOARD
```

The one-time code enrolls the computer. Its device credential is saved with owner-only permissions and reconnects automatically. The gateway stores only a hash of that credential. Generate another command from the same signed-in account to replace the paired computer.

On Linux, the included installer pairs once and creates a systemd user service:

```sh
remote/install-host-service.sh \
  --gateway https://builder.example.com \
  --code CODE_FROM_DASHBOARD \
  --workspace /path/to/project
```

The browser is a chat manager: create and switch between conversations, keep separate drafts, search, rename, archive/restore, and export transcripts. Up to **four chats can run independently**, each with its own live output, approval card, pause, and retry controls. Each run chooses its permissions: ask before changes, auto-approve everything, or read only; a host started read-only cannot be raised from the browser. You can also cancel a saved turn, rewind the last turn, and compact context while keeping the original transcript. Choose a configured model profile when starting a new chat.

The directory passed with `-C` is the host **workspace root**. Each new chat picks a folder under that root from the browser, so separate projects get separate chats without separate host processes; a saved chat keeps its folder. Chats created by the CLI inside the root appear in the same list. Close an already-open terminal session before driving that same session remotely; its lock is respected. A browser disconnect leaves runs active.

Pangolin SSO controls browser access. The declarative resource allows only `/api/gateway/connect` to bypass interactive SSO; Builder authenticates that route with the one-time pairing code or saved device credential. The gateway container has no source-tree, provider-key, Builder-home, or Docker-socket mount. Shell tools still run with the host user's permissions.

For a browser on the same computer with no Docker or Pangolin, `builder -C /path/to/project remote` still serves the direct interface at `http://127.0.0.1:7432` and uses the printed local token file.

[Complete remote setup and API behavior →](docs/REMOTE_CONTROL.md)

## What survives a failure?

- **Committed conversations:** user messages are saved before model requests; complete answers and tool results are stored transactionally.
- **A broken model connection:** bounded retries resend committed input. Partial output stays provisional and cannot trigger tools.
- **Interrupted execution:** a tool with an uncertain outcome stops for inspection. Builder never automatically replays it, including in auto mode.
- **A full context window:** automatic compaction normally starts at 75% of the configured budget. It uses labelled summaries and retains every original transcript row. Uncompactable input stops without silently dropping history.
- **A browser disconnect:** the host continues running. Reconnecting reads status and saved history; it does not submit the task again. Use **Pause** to stop an active run.

SQLite data and tool output are local plaintext. Relevant conversation and memory content is sent to your selected model endpoint. File boundaries and approval prompts are not an OS sandbox. [Persistence and recovery design →](ARCHITECTURE.md)

## Documentation

| Guide | Covers |
| --- | --- |
| [CLI and configuration](docs/CLI_GUIDE.md) | Commands, permissions, imports, auth, context, and settings |
| [Remote control](docs/REMOTE_CONTROL.md) | Docker, Pangolin/proxies, networking, tokens, and recovery |
| [Memory](docs/MEMORY.md) | Local and remote embeddings, provenance, revisions, and retrieval |
| [Code index](docs/CODE_INDEX.md) | Structural chunks, filesystem updates, hybrid ranking, freshness, and controls |
| [Agent roadmap](docs/AGENT_ROADMAP_2026.md) | September 2026 capability audit, current research, and prioritized repository-intelligence work |
| [Research runtime](docs/RESEARCH_RUNTIME.md) | Evidence, candidate experiments, verification, and feature switches |
| [Architecture](ARCHITECTURE.md) | Crate boundaries and persistence invariants |
| [Model evaluations](docs/LLM_EVALUATIONS.md) | Live fixtures and their limitations |
| [Verification results](docs/VERIFICATION.md) | Checks run for this implementation and remaining limits |

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
python3 tests/terminal_smoke.py target/release/builder
```

Tests cover failed streams, retries, storage reopening, session locks, history preservation, permissions, interrupted tools, memory provenance, embeddings, and the remote HTTP interface. Real-model checks are opt-in and use disposable fixtures; deterministic tests do not establish that every model will solve every coding task.

Four crates keep the dependency graph one-way: application → provider/tools → core. The browser is plain HTML/CSS/JavaScript; its Docker image uses nginx and needs no Node build step. MIT licensed.
