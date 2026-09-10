#!/bin/sh
# Install and start `builder remote` as a systemd user service. No sudo needed.
#
#   remote/install-host-service.sh --origin https://builder.example.com \
#       --workspace /path/to/project [--proxy-network NETWORK] [--listen HOST:PORT]
#
# --proxy-network is the Docker network your proxy/tunnel container (Pangolin
# newt, Traefik, Caddy) is on; the web container joins it and is reachable there
# as http://builder-web:8080. The script also writes remote/.env and runs
# `docker compose up -d --build` (skip with --no-compose). Re-running with flags
# updates the files; without flags existing values are kept.
# Uninstall: systemctl --user disable --now builder-remote; cd remote && docker compose down
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
config_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/builder
env_file=$config_dir/remote.env
unit_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user
origin='' workspace='' listen='' proxy_network='' compose=yes
while [ $# -gt 0 ]; do
    case "$1" in
        --origin) origin=$2; shift 2 ;;
        --workspace) workspace=$(CDPATH= cd -- "$2" && pwd); shift 2 ;;
        --listen) listen=$2; shift 2 ;;
        --proxy-network) proxy_network=$2; shift 2 ;;
        --no-compose) compose=no; shift ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) printf 'Unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done
command -v systemctl >/dev/null 2>&1 || { echo 'systemd user services are required (Linux).' >&2; exit 1; }
builder_bin=$(command -v builder 2>/dev/null || true)
[ -n "$builder_bin" ] || { echo 'builder is not on PATH; run ./install.sh first.' >&2; exit 1; }

# Default listen address: the docker0 gateway, which every Compose network can reach.
if [ -z "$listen" ] && [ ! -f "$env_file" ]; then
    gateway=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || true)
    listen="${gateway:-172.17.0.1}:7432"
fi

mkdir -p "$config_dir" "$unit_dir"
if [ ! -f "$env_file" ]; then
    (umask 077 && cp "$here/remote.env.example" "$env_file")
    printf 'Created %s\n' "$env_file"
fi
chmod 600 "$env_file"
set_value() { # file key value
    if grep -q "^$2=" "$1"; then
        sed -i "s|^$2=.*|$2=$3|" "$1"
    else
        printf '%s=%s\n' "$2" "$3" >> "$1"
    fi
}
[ -n "$origin" ] && set_value "$env_file" BUILDER_REMOTE_ORIGIN "$origin"
[ -n "$workspace" ] && set_value "$env_file" BUILDER_REMOTE_WORKSPACE "$workspace"
[ -n "$listen" ] && set_value "$env_file" BUILDER_REMOTE_LISTEN "$listen"
listen=$(sed -n 's/^BUILDER_REMOTE_LISTEN=//p' "$env_file")

# Compose settings: the container must dial the same address the host listens on.
compose_env=$here/.env
[ -f "$compose_env" ] || cp "$here/.env.example" "$compose_env"
case "$listen" in
    0.0.0.0:*|'[::]':*) set_value "$compose_env" BUILDER_UPSTREAM "host.docker.internal:${listen##*:}" ;;
    *) set_value "$compose_env" BUILDER_UPSTREAM "$listen" ;;
esac
if [ -n "$proxy_network" ]; then
    docker network inspect "$proxy_network" >/dev/null 2>&1 || { printf 'Docker network %s does not exist (see: docker network ls)\n' "$proxy_network" >&2; exit 1; }
    set_value "$compose_env" BUILDER_PROXY_NETWORK "$proxy_network"
    set_value "$compose_env" COMPOSE_FILE compose.yaml:proxy-network.yaml
fi

sed "s|@BUILDER_BIN@|$builder_bin|" "$here/builder-remote.service" > "$unit_dir/builder-remote.service"
systemctl --user daemon-reload

if grep -qE '^BUILDER_REMOTE_(ORIGIN=https://builder.example.com|WORKSPACE=/path/to/your/project)$' "$env_file"; then
    printf 'Edit %s (origin and workspace), then run:\n  systemctl --user enable --now builder-remote\n' "$env_file"
    exit 0
fi
systemctl --user enable --now builder-remote
if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]; then
    printf 'Service runs only while you are logged in; to keep it running after logout:\n  loginctl enable-linger %s\n' "$USER"
fi
if [ "$compose" = yes ] && command -v docker >/dev/null 2>&1; then
    (cd "$here" && docker compose up -d --build)
fi
sleep 1
systemctl --user --no-pager --lines=0 status builder-remote || true
journalctl --user -u builder-remote -n 30 --no-pager -o cat 2>/dev/null | grep -E '^(Remote control|Workspace root|Token file):' | tail -3
