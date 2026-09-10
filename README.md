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

Replace the URL and model ID with those from your server, then run `builder doctor`. It checks configuration, storage, and model discovery; `builder models` lists the advertised model IDs. A successful discovery check does not prove generation or tool calling works.

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

Start `builder remote` from a shell or service that has `BUILDER_BASIC_AUTH` set. Model credentials belong to the **host process**; they are not Docker web-container settings. A Basic Auth login protecting your public web page is separate from the model endpoint's Basic Auth and Builder's host access token. [Remote proxy setup →](docs/REMOTE_CONTROL.md#tokens-and-permissions)

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

## Remote control

Run Builder on your host and open its interface from a browser. Docker serves the web interface and proxies requests to the host process. Code, model credentials, SQLite, embeddings, and tool execution remain with Builder on the host.

```text
Your browser
    │ HTTPS
    ▼
Pangolin / your reverse proxy
    │ private network
    ▼
Docker: Builder web interface
    │ authenticated HTTP over the local/private network
    ▼
builder remote on your host → workspace + model endpoint + SQLite
```

### Local browser, no Docker required

```sh
builder -C /path/to/project remote
```

Open **http://127.0.0.1:7432**. Read the access token from the file printed by Builder and paste it into the login form. Leave **Remember this browser** checked to skip the login on later visits; Disconnect forgets it.

### Docker behind your reverse proxy

Two things run, and they listen on different ports:

| Piece | Where | Port | Who connects to it |
| --- | --- | --- | --- |
| `builder remote` host process | on your machine, outside Docker | 7432 | only the web container |
| `builder-web` container (nginx) | Docker, from `remote/compose.yaml` | 8080 | your reverse proxy / Pangolin |

The `http://127.0.0.1:7432` address from the local section is the host process bound to loopback; Docker cannot reach it, and it is not what your proxy targets. The container reaches the host over the Docker bridge, so it does not have to be in the same Compose project as Pangolin. The two only need to share a Docker network so the proxy can address the container by name.

**1. One command on the host** (Linux with systemd and Docker; no sudo). Run it from the Builder source folder, and replace the three example values with your own domain, project directory, and the Docker network your proxy container is on:

```sh
cd /path/to/builder            # the folder you cloned or extracted
docker network ls              # find the network Pangolin's newt / your proxy is on
remote/install-host-service.sh \
  --origin https://builder.example.com \
  --workspace /path/to/project \
  --proxy-network your-proxy-network
```

This starts `builder remote` as a `builder-remote` systemd user service listening on the docker0 gateway (`172.17.0.1:7432` by default), writes `remote/.env` with the matching upstream and network, and runs `docker compose up -d --build` for the web container. The `--workspace` directory is the root; each chat then picks a folder under it. Settings live in `~/.config/builder/remote.env` and `remote/.env`; edit and re-run the script, or `systemctl --user restart builder-remote`. Omit `--proxy-network` when the proxy runs on the host itself. Without systemd, do it by hand: `builder -C /path/to/project remote --listen 0.0.0.0:7432 --origin https://builder.example.com`, then `cd remote && docker compose up -d --build`.

**2. Proxy:** add a resource in Pangolin (or your proxy) for the origin from step 1, targeting **`http://builder-web:8080`**. A proxy running on the host itself uses `http://127.0.0.1:8080` instead and does not need the network override.

The browser is a chat manager: create and switch between conversations, keep separate drafts, search, rename, archive/restore, and export transcripts. Up to **four chats can run independently**, each with its own live output, approval card, pause, and retry controls. Each run chooses its permissions: ask before changes, auto-approve everything, or read only; a host started read-only cannot be raised from the browser. You can also cancel a saved turn, rewind the last turn, and compact context while keeping the original transcript. Choose a configured model profile when starting a new chat.

The directory passed with `-C` is the host **workspace root**. Each new chat picks a folder under that root from the browser, so separate projects get separate chats without separate host processes; a saved chat keeps its folder. Chats created by the CLI inside the root appear in the same list. Close an already-open terminal session before driving that same session remotely; its lock is respected. A browser disconnect leaves runs active.

**The token controls your host's Builder process.** Keep port 7432 on the private host/container network and expose only the web interface through HTTPS and your proxy's access controls. The container does not need your source tree, provider keys, home directory, or Docker socket mounted. Shell tools still run with the host user's permissions.

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
