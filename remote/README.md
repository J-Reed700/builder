<p align="center"><img src="../docs/assets/builder-mark.svg" width="72" alt="Builder logo"></p>
<h1 align="center">Builder Remote</h1>
<p align="center"><strong>Your coding agent, from any browser.</strong></p>
<p align="center">The gateway relays the interface. Your code, model credentials, conversations, and tools stay on your computer.</p>

![Builder remote interface](../docs/design/refinement/browser.png)

## Pick your setup

| I want to… | Use |
| :--- | :--- |
| Open Builder in a browser on the same computer | [Local mode](#local-mode-no-docker) |
| Reach Builder through a reverse proxy or tunnel | [Gateway quick start](#gateway-quick-start) |
| Use Pangolin/Newt container discovery | [Pangolin recipe](#pangolinnewt-recipe) |
| Keep the host connected after logout on Linux | [Install the host service](#keep-the-host-online-on-linux) |
| Customize networking or security | [Advanced setup](../docs/REMOTE_CONTROL.md) |

## Local mode—no Docker

If the browser and Builder are on the same computer, this is all you need:

```sh
builder -C /path/to/project remote
```

Open `http://127.0.0.1:7432` and paste the token from the file path printed in
the terminal. The server binds to loopback by default.

## Gateway quick start

Builder Gateway works behind any HTTPS reverse proxy or authenticated tunnel
that can forward WebSockets and an identity header. That includes setups built
with Caddy, Nginx, Traefik, HAProxy, Pangolin/Newt, or a tunnel provider.

```text
browser ──HTTPS──▶ your proxy / tunnel ──▶ Builder Gateway
                                              ▲
                                              │ outbound WSS
                                              │
                                      Builder on your computer
```

You need [Builder installed](../README.md#1-install) on the computer that owns
the workspace, Docker Compose on the gateway host, and an HTTPS URL your browser
can reach.

### 1. Start the gateway

From this directory, create `.env` and set the public URL plus the header your
proxy uses for the signed-in user:

```sh
cp .env.example .env
```

```dotenv
BUILDER_GATEWAY_ORIGIN=https://builder.example.com
BUILDER_GATEWAY_AUTH_HEADER=X-Authenticated-User
COMPOSE_FILE=compose.yaml
```

```sh
docker compose up -d
docker compose ps builder-gateway
```

Point the proxy at `http://127.0.0.1:8080`, or attach the gateway to the proxy's
Docker network with `proxy-network.yaml`. Keep port 8080 private.

Your proxy configuration must:

- terminate HTTPS and forward WebSocket upgrades;
- authenticate browser routes and pass a stable user value in the configured
  identity header, replacing any client-supplied value; and
- allow `/api/gateway/connect` through without an interactive login. Builder
  authenticates that connector with its pairing code or saved device key.

### 2. Pair your computer

Open `https://builder.example.com`, sign in through your proxy, and click
**Generate connect command**. Run the displayed command in the workspace you
want Builder to control:

```sh
cd /path/to/project
builder remote connect https://builder.example.com --code CODE_FROM_DASHBOARD
```

The invitation expires after ten minutes. After pairing, Builder stores a
private device credential, so future connections do not need another code:

```sh
builder -C /path/to/project remote connect https://builder.example.com
```

Return to the browser and start a conversation.

## Pangolin/Newt recipe

Pangolin users can add the supplied labels to an existing Compose stack with
container discovery enabled. Put `BUILDER_GATEWAY_ORIGIN` and `BUILDER_DOMAIN`
in that stack's `.env`, then run:

```sh
docker compose \
  -f compose.yml \
  -f /path/to/builder/remote/compose.yaml \
  -f /path/to/builder/remote/pangolin-labels.yaml \
  up -d builder-gateway
```

Newt can then reach `builder-gateway:8080` on the shared Compose network. The
labels enable SSO and exempt only the authenticated Builder connector route.

To build the image from this checkout instead of pulling it:

```sh
docker compose \
  -f compose.yaml \
  -f compose.build.yaml \
  build builder-gateway
```

## Keep the host online on Linux

The included installer pairs the computer and creates a systemd user service.
It does not need sudo:

```sh
remote/install-host-service.sh \
  --gateway https://builder.example.com \
  --code CODE_FROM_DASHBOARD \
  --workspace /path/to/project
```

Useful service commands:

```sh
systemctl --user status builder-remote
journalctl --user -u builder-remote -f
systemctl --user restart builder-remote
```

If your model profile reads credentials from environment variables, add them to
`~/.config/builder/remote.env` and restart the service. The installer will tell
you if systemd lingering must be enabled to stay connected after logout.

## Update

Update the gateway container:

```sh
docker compose pull builder-gateway
docker compose up -d builder-gateway
```

Update the host by pulling the repository and running `./install.sh` again.
Pairing credentials and conversations are preserved.

## Security in one minute

- Keep the gateway behind an identity-aware reverse proxy or authenticated
  tunnel.
- Do not publish port 8080 directly to the internet. The supplied Compose file
  binds it to `127.0.0.1` by default.
- Only the single-use connector path bypasses browser SSO; it still requires a
  pairing code or saved device credential.
- The gateway stores pairing state in `builder-gateway-data`. It does not mount
  the host filesystem, Docker socket, Builder data, or model credentials.
- Browser permissions do not sandbox approved shell commands. Those run as the
  account that started Builder.

Read the full [deployment, threat model, recovery, and troubleshooting guide](../docs/REMOTE_CONTROL.md).

## Quick troubleshooting

| Problem | Fix |
| :--- | :--- |
| Gateway page does not open | Confirm DNS/TLS, the proxy target, and `docker compose ps builder-gateway`. |
| Dashboard says the host is offline | Start `builder remote connect` or inspect `journalctl --user -u builder-remote`. |
| Pairing code is rejected | Generate a new code; invitations expire after ten minutes. |
| Connector gets HTTP 302, 401, or 403 | Ensure only `/api/gateway/connect` bypasses interactive SSO. |
| Browser gets HTTP 401 | Confirm the proxy forwards the header named by `BUILDER_GATEWAY_AUTH_HEADER`. |
| Model credentials are missing | Add the required variables to `~/.config/builder/remote.env` and restart the service. |
