# Remote control

[Back to README](../README.md)

For the shortest installation path, start with the
[Builder Remote quick start](../remote/README.md). This page documents the full
deployment, security, recovery, and API behavior.

Builder Gateway is the public control plane for a Builder process running on your computer. The gateway lives beside Pangolin/Newt or another reverse proxy. The computer makes an outbound WebSocket connection to it; no container needs to reach a host port.

```text
Browser ──HTTPS──> Pangolin/Newt ──HTTP──> Builder Gateway
                                               ▲
                                               │ outbound WSS
                                               │
                                      Builder host process
                                      workspace · model · SQLite · tools
```

The gateway contains the browser assets, pairing state, one hashed device credential, and a bounded request relay. The host injects its local API credential after each request reaches the computer. Workspace contents, provider credentials, conversations, memory, tool output, and the Docker socket never enter the gateway container.

## Add the image to an existing Pangolin/Newt Compose stack

The production Compose definition is deliberately image-only. It can be merged into a stack that is already running Newt:

```sh
# Run from the directory containing the existing compose.yml. Put the two
# BUILDER_* values below in this stack's .env first.
docker compose -f compose.yml \
  -f /path/to/builder/remote/compose.yaml \
  -f /path/to/builder/remote/pangolin-labels.yaml \
  up -d builder-gateway
```

```dotenv
BUILDER_GATEWAY_ORIGIN=https://builder.example.com
BUILDER_DOMAIN=builder.example.com
```

Compose pulls `ghcr.io/j-reed700/builder-gateway:latest`; it does not need the Builder source tree or a Rust toolchain. Because the gateway becomes part of the existing Compose project, it joins that project's default network and Newt can target `http://builder-gateway:8080` by service name. The gateway has no Docker-socket mount; only Newt needs its normal container-discovery access.

The files are conventional Compose fragments, so copying the `builder-gateway` service and `builder-gateway-data` volume directly into the existing `compose.yml` works the same way. Pin the image to a release tag instead of `latest` when the stack requires deterministic upgrades, then run `docker compose pull builder-gateway && docker compose up -d builder-gateway` to upgrade it.

`pangolin-labels.yaml` declares an HTTP resource for `BUILDER_DOMAIN`, targets `builder-gateway:8080`, and enables Pangolin SSO. It adds one path rule: `/api/gateway/connect` may bypass interactive SSO because command-line WebSocket clients cannot complete a browser login. Builder Gateway still authenticates every connection on that path using a single-use invitation or a saved 256-bit device credential.

Container-label discovery must be enabled for the Newt site. If Newt and the gateway cannot share the default network because they are separately managed, set `BUILDER_PROXY_NETWORK` and include the supplied network override:

```sh
BUILDER_PROXY_NETWORK=pangolin-network docker compose \
  -f remote/compose.yaml \
  -f remote/proxy-network.yaml \
  -f remote/pangolin-labels.yaml \
  up -d builder-gateway
```

## Start a standalone gateway stack

Copy the example environment file and set your public URL and hostname:

```sh
cp remote/.env.example remote/.env
```

```dotenv
BUILDER_GATEWAY_ORIGIN=https://builder.example.com
BUILDER_DOMAIN=builder.example.com
COMPOSE_FILE=compose.yaml:pangolin-labels.yaml
```

Start the gateway from the `remote` directory:

```sh
cd remote
docker compose up -d
```

The sample `.env` selects `compose.yaml` and `pangolin-labels.yaml` through `COMPOSE_FILE`. The service listens on container port 8080, publishes it on loopback for local diagnostics, and keeps its device record in the `builder-gateway-data` volume. It runs as an unprivileged user with `no-new-privileges` and has no host or Docker-socket mount.

Pangolin forwards the authenticated identity as `Remote-User`. The gateway requires that header by default, binds the paired computer to that identity, and rejects other signed-in users. Configure the Pangolin resource's roles or user allowlist to decide who may initially claim it. Do not expose the container directly on a public interface that bypasses Pangolin.

For a reverse proxy with another identity header, set `BUILDER_GATEWAY_AUTH_HEADER` to that header name. `BUILDER_GATEWAY_AUTH_HEADER=none` is intended only for a loopback-bound local test.

Release tags publish the image as `ghcr.io/j-reed700/builder-gateway`. A source checkout can build the same image by adding the opt-in build override:

```sh
docker compose -f remote/compose.yaml -f remote/compose.build.yaml build builder-gateway
```

## Pair a computer

Open `BUILDER_GATEWAY_ORIGIN` and sign in through Pangolin. The dashboard shows **Connect your computer** while the host is offline. Click **Generate connect command**, open a terminal in the project you want Builder to control, and run the command it displays:

```sh
cd /path/to/project
builder remote connect https://builder.example.com --code 64_CHARACTER_CODE
```

Invitations expire after ten minutes and are consumed by one device identity. If the acknowledgement is lost, the same device can retry the code during that window and receive the same credential. A different device cannot reuse it.

After enrollment, the host saves a gateway-specific credential under `remote-gateways/` in the Builder home directory. On Unix, the directory is mode 0700 and credential is mode 0600. Future starts omit `--code`:

```sh
builder -C /path/to/project remote connect https://builder.example.com
```

The connector retries network failures with a bounded delay of one to thirty seconds. Authentication, TLS, protocol, and Pangolin routing failures stop with a concrete error instead of retrying forever. Reconnection never resubmits an API request.

Generating a new invitation from the same Pangolin identity replaces the paired device credential. This is also the current recovery flow if the host credential is lost. One gateway controls one paired computer; the workspace root can contain many projects, and each new chat chooses a subfolder.

## Keep the connector running on Linux

The included installer performs pairing, writes a private environment file, and installs a systemd user service without sudo:

```sh
remote/install-host-service.sh \
  --gateway https://builder.example.com \
  --code 64_CHARACTER_CODE \
  --workspace /path/to/project
```

The service runs `builder remote connect` without keeping the invitation code. Re-run the installer to update the gateway or workspace. Add any environment variables required by model profiles to `~/.config/builder/remote.env`, then restart it:

```sh
systemctl --user restart builder-remote
journalctl --user -u builder-remote -f
```

The installer reports when systemd lingering is disabled. Enable it if Builder should stay online after logout. Remove the service with `systemctl --user disable --now builder-remote`.

## Direct local mode

The original direct interface remains useful for a browser on the same computer:

```sh
builder -C /path/to/project remote
```

Open `http://127.0.0.1:7432`, read the token file path Builder prints, and paste that token into the page. Direct mode binds to loopback by default. `--listen` and `--origin` remain available for explicitly managed private proxy setups.

## Browser behavior and permissions

The web interface manages saved chats, folders below the workspace root, profiles, drafts, live output, approvals, pause/retry, rename, archive/restore, rewind, explicit compaction, and transcript export. It supports up to four concurrent chats. A session already open in another Builder process remains locked.

Each run chooses ask, trust, or read-only permissions. Starting the host with `--approval read-only` is a hard ceiling that the browser cannot raise. Gateway ownership authenticates access; it is not an operating-system sandbox. Tool processes use the account running Builder.

Pangolin identity is required for every browser API request. Browser mutations also require the exact configured gateway Origin. The connector route accepts only the versioned Builder wire protocol, limits messages and request concurrency, and authenticates credentials in constant time. Pairing and device secrets are never placed in gateway logs by Builder.

## Failure and recovery rules

- The browser and gateway never replay a POST. If the gateway loses the host before receiving its acknowledgement, it returns an explicit uncertain-result error and tells the user to inspect the chat.
- Builder commits the user message before inference and retains its existing request-ID deduplication, session locks, approval binding, tool claims, and uncertain-execution handling.
- Reconnecting only restores the transport and reads status. It does not resume an unfinished turn or repeat a tool.
- Gateway requests are limited to 128 KiB, responses to 8 MiB, pending requests to 32, and a twelve-second relay deadline. The host keeps its own stricter per-operation bounds.
- A browser disconnect leaves accepted host work running. Use **Pause run** to request cancellation.
- Original and rewound conversation rows remain in SQLite. Display and export limits produce explicit errors rather than deleting history.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Dashboard says the host is offline | Start `builder remote connect` on the paired computer and inspect its terminal or service log. |
| Connector receives HTTP 302, 401, or 403 | Ensure Pangolin's allow rule matches only `/api/gateway/connect`; the rest of the resource should use SSO. |
| Dashboard receives 401 | Confirm Pangolin SSO is enabled and forwards `Remote-User`, matching `BUILDER_GATEWAY_AUTH_HEADER`. |
| Dashboard says another user owns the gateway | Sign in with the Pangolin account that paired it, or remove the gateway data volume only if you intend to erase enrollment. |
| Pairing code is rejected | Generate a fresh command; codes expire after ten minutes and cannot enroll a second device identity. |
| Send reports an uncertain result | Inspect status and durable chat history. Do not resubmit until you know whether Builder accepted the original request. |
| Session is already open | Exit the terminal conversation or other Builder process that holds its lock. |
| Model credentials are missing in the service | Add the profile's required environment variables to `~/.config/builder/remote.env` and restart the service. |

Stopping the gateway container does not delete its named volume. `docker compose down -v` removes the saved gateway enrollment and requires pairing again; it never touches the host workspace or Builder database.

## Gateway endpoints

| Route | Purpose |
| --- | --- |
| `GET /healthz` | Container health probe; returns no private state. |
| `GET /api/gateway/status` | Authenticated browser view of paired and connected state. |
| `POST /api/gateway/invitations` | Creates a ten-minute, single-device pairing command for the authenticated identity. |
| `GET /api/gateway/connect` | WebSocket upgrade for the Builder host; bypasses interactive SSO and requires protocol credentials. |
| `/api/*` | Authenticated, bounded relay to the connected host's existing remote API. |

The direct host API retains its existing routes and response contracts. A successful `POST /api/run` returns 202 after the host accepts it. Transport errors are never an idempotency guarantee.
