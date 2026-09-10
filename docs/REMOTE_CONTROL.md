# Remote control

[Back to README](../README.md)

Builder's remote interface drives the same application engine as the CLI. The host process owns the workspace, provider connection, memory, journal, and session locks. The Docker container serves static HTML/CSS/JavaScript and proxies `/api/` to that process.

## Quick local check

```sh
builder -C /path/to/project remote
```

Open `http://127.0.0.1:7432`, read the token file whose path Builder prints, and paste its contents into **Host access token**. Start with a simple question and verify the answer appears in saved history. You can run this without Docker.

Defaults: loopback port 7432, the `-C` directory as the workspace root, the current default profile, and per-action approval. `--home`, `--profile`, `--approval read-only`, `--auto`, and `--max-rounds` work with remote control. Pipeline settings come from saved configuration; use `builder config pipeline set` before starting the host. The host reads `~/.config/builder/config.toml` (or its XDG/`--home` override). Config and memory settings are loaded for each run. Literal credential/profile changes apply to subsequent runs; restart the host process to pick up changed environment-variable values.

The web interface manages multiple saved chats and up to four concurrent runs, with independent drafts, live output, approvals, and pause/retry controls. It does not attach to the input of an already-running terminal process. Exit that conversation in the terminal to release its lock, then reopen it in the browser.

The `-C` directory is the **workspace root**. Every new chat chooses a folder at or below that root with the **Folder** control next to the model profile; the picker lists real subdirectories only (hidden directories, `node_modules`, `target`, and `__pycache__` are skipped) and cannot leave the root. The chat's tools, approvals, `AGENTS.md` instructions, and memory scope all use that folder, exactly as `builder -C folder` would. A saved chat keeps its folder. The sidebar lists every chat under the root, including ones created by the CLI in those directories, and shows each chat's folder. Directories outside the root still require a separate host process.

## Managing chats

- **New conversation** opens a fresh composer. Choose a configured model profile and a folder before the first message; the folder defaults to the last one you picked in this tab. A host started with `--profile` restricts the profile choice.
- On a phone the sidebar collapses into a top bar: **Chats** opens the conversation list as a full-screen drawer, dialogs such as the folder picker open as bottom sheets with a scrolling list, and the composer controls stack full width. Enter inserts a new line on touch keyboards; tap **Send** to run the task.
- Click a chat in the sidebar to switch. Running and approval badges stay visible for other chats. Each chat has its own unsent draft in this tab; reloading or disconnecting clears these unsent drafts. Rewinding also saves its recovered prompt durably on the host.
- **Search chats** matches names or session IDs. Use **Active chats / Archived chats** to filter the list. **Rename** changes the name; **Archive / Restore chat** hides or restores it without deleting messages, memory, or tool records. Restore an archived chat before sending another message.
- **Pause run** stops only the selected chat. After it stops, **Retry saved turn** continues its unfinished turn, or **Cancel saved turn** closes it without replaying work. You can also send a new instruction.
- **Rewind last turn** archives that turn and restores its user prompt to the composer. Review the confirmation: it replaces this chat's unsent draft and does not undo files or commands. Original rows remain available under **Include rewound messages**.
- **Compact context** explicitly summarizes older context without continuing the task. Original messages remain available in history and export.
- **Export transcript** prepares a JSON download link and copyable text, including rewound rows when that checkbox is selected. Browser exports stop with an explicit error above 5,000 messages or 32 MiB; use `builder export` on the host for larger transcripts.
- **Enter** sends; **Shift + Enter** inserts a newline. Tool exchanges expand in place, and fenced code blocks have a Copy button.

Each chat has separate conversation state and approvals, but chats in the same folder use the **same files**. They are not separate checkouts. Coordinate tasks that edit the same files. Model endpoints must also have capacity for concurrent requests.

On upgrade, the host migrates the session database to schema 6 to store chat archive state. Stop older Builder processes first and update the host binary together with the web image. Older binaries reject this newer schema; archive/restore does not delete history.

## Docker Desktop: macOS or Windows

On the host:

```sh
builder -C /path/to/project remote \
  --listen 0.0.0.0:7432 \
  --origin https://builder.example.com
```

From the Builder distribution folder:

```sh
docker compose -f remote/compose.yaml up -d --build
docker compose -f remote/compose.yaml logs --tail=50
```

The image is built locally as `builder-remote:local`; no published registry image is assumed. Docker Desktop provides [`host.docker.internal`](https://docs.docker.com/desktop/features/networking/) for reaching the host. The Compose file also supplies the `host-gateway` mapping used by Docker Engine on Linux. The upstream is `host.docker.internal:7432` by default.

For a local Docker-only trial, use `--origin http://localhost:8080` and visit that exact URL. `localhost` and `127.0.0.1` are different browser origins.

The explicit `0.0.0.0` bind lets the container reach Builder. Restrict port 7432 to the container/private network with your host firewall. Do not route the public reverse proxy directly to 7432. If a private host address is available, bind that specific address instead.

## Docker Engine: Linux

The same Compose command works with a host listener reachable from Docker's bridge. Bind the docker0 gateway rather than every interface when possible:

```sh
# 172.17.0.1 is the usual docker0 gateway; check with:
#   docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}'
builder -C /srv/project remote \
  --listen 172.17.0.1:7432 \
  --origin https://builder.example.com

BUILDER_UPSTREAM=172.17.0.1:7432 \
  docker compose -f remote/compose.yaml up -d --build
```

The docker0 gateway is a host address, so containers on other Compose networks (`172.18.0.0/16` and so on) reach it too; firewalld's `docker` zone allows this by default. With rootless Docker or a custom network, set `BUILDER_UPSTREAM` to the host address reachable from that network. The value is **host:port**, without a scheme or path. Container startup resolves the hostname to IPv4 (including `/etc/hosts` mappings) to match an IPv4 host listener. IPv6-only upstreams are not supported by this image. Recreate the container if the host address changes.

### Host process as a systemd user service

Running `builder remote` by hand means it stops when the terminal closes. The repository ships a user unit and an installer; run it from the Builder source folder with your own values:

```sh
remote/install-host-service.sh \
  --origin https://builder.example.com \
  --workspace /path/to/project \
  [--proxy-network NETWORK] [--listen 172.17.0.1:7432] [--no-compose]
```

The installer copies `remote/builder-remote.service` to `~/.config/systemd/user/` with the absolute path of your `builder` binary, creates `~/.config/builder/remote.env` (mode 0600) from `remote/remote.env.example`, and runs `systemctl --user enable --now builder-remote`. It also writes `remote/.env` (`BUILDER_UPSTREAM` matching `--listen`, plus `BUILDER_PROXY_NETWORK` and `COMPOSE_FILE` when `--proxy-network` is given) and runs `docker compose up -d --build` unless `--no-compose`. `--listen` defaults to the docker0 gateway on first install. The env file holds:

| Variable | Meaning |
| --- | --- |
| `BUILDER_REMOTE_WORKSPACE` | Directory exposed to the browser. Choose a project, not `/`. |
| `BUILDER_REMOTE_LISTEN` | `--listen` address; must match `BUILDER_UPSTREAM` in `remote/.env`. |
| `BUILDER_REMOTE_ORIGIN` | Exact public origin, e.g. `https://builder.example.com`. |
| `BUILDER_REMOTE_ARGS` | Optional extra flags such as `--approval read-only` or `--profile local`. |
| `BUILDER_HOME`, `BUILDER_BASIC_AUTH` | Optional; the service only sees variables from this file. |

Edit the file, then `systemctl --user restart builder-remote`. Logs: `journalctl --user -u builder-remote -f`. The token file path is printed at startup. The unit stops Builder with SIGINT, the same clean shutdown as Ctrl+C. Enable lingering (`loginctl enable-linger $USER`) so the service also runs while you are logged out; the installer tells you if it is off. Remove with `systemctl --user disable --now builder-remote`.

## Pangolin, WireGuard, and reverse proxies

Keep your existing tunnel, DNS, authentication, and TLS setup. Add one service pointing to the web container:

1. Choose a dedicated origin such as `https://builder.example.com`.
2. Pass that exact origin to `builder remote --origin` (or `BUILDER_REMOTE_ORIGIN`), with no trailing slash or path.
3. Route the full origin to the container's port **8080**. Path-prefix hosting, such as `/builder/`, is not supported.
4. Preserve the browser's `Origin` and `X-Builder-Token` request headers. Use your proxy's own authentication as well. The Builder token uses a dedicated header so it can coexist with gateway `Authorization` headers and cookies.
5. Keep the container-to-host connection on the same machine or a trusted private/WireGuard network. It uses HTTP; TLS terminates at your public reverse proxy.

The web container does **not** need to live in the same Compose project as Pangolin. If the reverse proxy runs **on the host**, target `http://127.0.0.1:8080` with the default Compose settings. If it runs **in Docker** (Pangolin's `newt`, Traefik, Caddy), a proxy container's `localhost` refers to itself, not your host, so attach the web service to the proxy's network with the shipped override and target `http://builder-web:8080`:

```sh
docker network ls                      # find the network newt / your proxy is on
remote/install-host-service.sh --origin https://builder.example.com \
  --workspace /path/to/project --proxy-network <that network>
```

Or by hand: copy `remote/.env.example` to `remote/.env`, set `BUILDER_PROXY_NETWORK`, `COMPOSE_FILE=compose.yaml:proxy-network.yaml`, and `BUILDER_UPSTREAM` to the `--listen` address, then `cd remote && docker compose up -d --build`.

`remote/proxy-network.yaml` joins the external network named by `BUILDER_PROXY_NETWORK` in addition to the project's own default network, so the service name `builder-web` resolves from the proxy container. `remote/.env` is read automatically by Compose and is gitignored. Without `COMPOSE_FILE`, pass both files explicitly: `docker compose -f remote/compose.yaml -f remote/proxy-network.yaml up -d --build`. In Pangolin, create an HTTP resource for the origin with target `builder-web`, port `8080`, on the site whose `newt` shares that network.

If the proxy reaches a private host interface instead, set `BUILDER_WEB_BIND` to that specific address. `BUILDER_WEB_PORT` changes the host-side published port. The container always listens on 8080.

The nginx proxy has [automatic upstream retries disabled](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_next_upstream). The browser also never retries a mutation automatically. UI polling uses ordinary HTTP requests, so no WebSocket or SSE upgrade configuration is needed.

## Tokens and permissions

There are three independent connections to configure:

| Connection | Authentication belongs here |
| --- | --- |
| Host Builder → model server | Put `Authorization = { env = "BUILDER_BASIC_AUTH" }` in that model profile's `headers` in `~/.config/builder/config.toml`. The host environment supplies the full `Basic …` value. [Complete example](../README.md#custom-endpoint-with-http-basic-auth). |
| Browser → your public reverse proxy | Configure Basic Auth or your existing login at Pangolin/the gateway. The Docker image does not create a gateway username/password. |
| Browser → Builder API through Docker | Enter the host's `remote-token` value in the Builder login screen; requests use `X-Builder-Token`. |

Your gateway's HTTP `Authorization` header can coexist with `X-Builder-Token`. Do not use a model API key in the Builder login form. Model-server credentials stay on the host; do not add them to Compose or the browser. A host running as a service must receive its environment variables in that service configuration, and must restart after those environment values change.

At first startup, Builder creates a random 64-character token at `remote-token` under its home directory. On Unix it requires a regular file with owner-only permissions (`0600`). It prints the file path, not the token. Use your normal file reader to copy it; do not put it in a URL or in the Docker image.

With **Remember this browser** checked (the default), the browser keeps the token in its local storage and reconnects automatically on later visits; **Disconnect** forgets it, and a rejected saved token is dropped. Unchecked, the token is held in tab memory only and reloading clears it. A remembered token has the same power as the token file, so keep it to browsers you control and rely on your proxy's own login in front of the page. All API routes require the token. Browser mutations additionally require the exact configured Origin; there is no cookie authentication or permissive CORS. Provider credentials never enter the web configuration. Static HTML is public but contains no conversation data or access token.

The token is single-user administrative access to Builder within the exposed workspace. Anyone holding it can read that workspace's saved conversations and direct its tools, including approving shell commands. This is not a multi-user service or an OS sandbox. The **Permissions** control in the composer chooses the mode for each run: *Ask before changes*, *Auto-approve all tools* (the same as `--auto`), or *Read only*; the host's `--approval` is the default. Start the host with `--approval read-only` if you only want remote inspection: that is a hard ceiling the browser cannot raise.

To rotate the token, stop the host process, remove its `remote-token` file, and restart. Disconnect existing browser tabs and enter the new token. Multiple host processes sharing one Builder home share that token; use separate `--home` directories if separate credentials are required.

Approval cards show the entire proposed action. Each response is bound to the current run ID and a one-time approval ID. Wrong, reused, expired, or cancelled approvals are rejected. Unanswered requests are denied after two minutes. Actions larger than the 128 KiB approval-display limit are denied, never shown partially and then authorized.

## Running, pausing, and recovery

- Up to four chats run concurrently per host, with at most one run per chat. A fifth run, or a competing request for the same chat, returns busy. The existing session OS lock also prevents a CLI/remote collision. Admission rejects excess runs before creating a new chat.
- A browser disconnect does not cancel work. Reconnect to inspect its current state. Use **Pause run** to cancel the host future and preserve committed messages. The composer allows a new follow-up once paused.
- **Retry saved turn** is explicit. The host never starts unfinished conversations merely because you opened or reconnected to them.
- If a tool was claimed but has no durable result after interruption, recovery records uncertainty and stops for inspection, even in auto mode. Inspect the workspace before choosing Retry again. Existing agent recovery rules remain authoritative.
- Live text is labelled provisional and cleared on a new attempt or at the end of a run. Only complete saved messages appear in history. A long live preview stops at 64 KiB and says so; it does not truncate the saved answer.
- A failed or lost POST response is not automatically resubmitted. The browser retains the draft and refreshes status; inspect saved history before sending it again. The host rejects repeated accepted request IDs for its lifetime (up to 4,096 accepted runs). That in-memory deduplication resets on host restart; it is not a durable idempotency contract for third-party clients. Never replay a mutation after an uncertain response or process restart.
- Successful runs perform optional memory maintenance while idle. A new foreground request for that chat cancels its maintenance before starting the next run. Other chats remain independent. Missing vectors and unfinished extraction remain eligible for a later pass.
- Shutdown cancels all workers. Tool subprocesses keep their existing cancellation/RAII cleanup. A host run is bounded to one hour, independent of the configured model-round budget.

## History and resource bounds

The session list is scoped to the canonical host workspace, 50 conversations per page. History serves original rows, 25 per page and at most 8 MiB per response; `next_before` loads earlier rows. The default includes original messages covered by compaction. **Include rewound messages** adds rows archived by explicit rewind. This changes only what the browser displays; active model context is unchanged.

Requests are capped at 128 KiB; prompts at 64 KiB. Live notices keep 32 entries per chat; the host retains the latest run status for up to 64 chats. Older chat transcripts remain in SQLite and can be reopened. The browser holds at most 64 unsent drafts in memory, each bounded by the composer limit. The browser limits a loaded transcript view and asks you to reopen it when that display limit is reached. Stored messages are never deleted to meet display bounds. Individual saved messages above the browser's 8 MiB limit require local inspection. The host bounds concurrent HTTP requests; nginx also rate-limits API traffic.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `502` from the container | Is `builder remote` running? Can the container reach `BUILDER_UPSTREAM`? Is the host listener bound to a reachable address? |
| `403` on Send or Allow | Match `--origin` to the exact browser scheme, hostname, and port. Preserve Origin at your proxy. |
| `401` | Use the token for this host/home directory, not a model API key. |
| Session is already open | Exit the terminal conversation or the other process that holds its lock. |
| No saved sessions | Check `--home` and `-C`; only chats whose folder is at or below the workspace root appear. |
| Changes never ask | Check the Permissions control for that run and whether the host was launched with `--auto`. Read-only mode denies changes automatically. |
| Memory is disabled or stale | Configure it on the host with `builder memory status` / `builder memory enable --local`; the next run reloads those settings. |
| Read-only token file or invalid permissions | Stop the host; restore owner-only permissions, or remove the token file and restart to rotate it. |
| `disk I/O error: Error code 522` | Fixed in this version: an older Builder opened the database file directly and lost its SQLite locks, so another Builder process (for example `builder doctor`) could truncate the write-ahead log under a live host. Update every Builder binary that shares the data directory. |

Stop the web container with `docker compose -f remote/compose.yaml down`. Stop the host with Ctrl+C. These commands do not remove the host's workspace, conversations, or memory.

## API sketch

All routes use `X-Builder-Token: TOKEN`. POST routes also require `Origin: EXACT_CONFIGURED_ORIGIN`.

| Route | Behavior |
| --- | --- |
| `GET /api/status` | `root` (the host workspace root; `workspace` is the same value), `approval_mode` (host default), `approval_locked` (host is read-only), `states` array of independent chat runs, `max_concurrent_runs`; `state` is the latest run for compatibility |
| `GET /api/folders?path=REL` | Subdirectories of `REL` under the root: `path`, `parent` (null at the root), `folders`, `limited` |
| `GET /api/profiles` | Available chat profile names/models, without credentials |
| `GET /api/sessions?offset=0&archived=false&search=TEXT` | Page of chats under the root; each carries its `workspace` and root-relative `folder` |
| `GET /api/sessions/UUID` | Session metadata, archive flag, pending-turn flag, active transcript tip, recovered draft |
| `POST /api/sessions/UUID` | `{ "action": "rename", "title": "..." }` or `{ "action": "archive", "archived": true }` |
| `POST /api/sessions/UUID` | `{ "action": "rewind", "expected_tip": SEQ }` or `{ "action": "cancel", "expected_tip": SEQ }`; stale tips are rejected |
| `GET /api/sessions/UUID/messages?before=SEQ&include_archived=false` | Original transcript page |
| `POST /api/run` | `{ "request_id": "UUID", "session": null, "profile": "local", "workspace": "apps/web", "operation": { "action": "message", "prompt": "..." } }`; `workspace` is root-relative (`""` or omitted means the root) and must be an existing directory inside it; optional `approval` is `ask`, `trust`, or `read-only` for this run |
| `POST /api/run` | `{ "request_id": "UUID", "session": "UUID", "operation": { "action": "retry" } }` |
| `POST /api/run` | `{ "request_id": "UUID", "session": "UUID", "operation": { "action": "compact" } }` |
| `POST /api/pause` | `{ "run_id": "UUID" }` |
| `POST /api/approval` | `{ "run_id": "UUID", "approval_id": "UUID", "allow": true }` |

`202` acknowledges an accepted run; poll status to learn whether execution completed. `409` reports conflicts, stale approvals, busy hosts, and unavailable sessions. Treat network/timeout errors as uncertain acceptance, inspect status, and never automatically repeat a POST.
