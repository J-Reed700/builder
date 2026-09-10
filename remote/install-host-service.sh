#!/bin/sh
# Pair Builder with a gateway and install its outbound connector as a systemd
# user service. No sudo or Docker networking configuration is required.
#
#   remote/install-host-service.sh --gateway https://builder.example.com \
#       --code CODE_FROM_DASHBOARD --workspace /path/to/project
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
config_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/builder
env_file=$config_dir/remote.env
unit_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user
gateway='' code='' workspace='' builder_home=''
while [ $# -gt 0 ]; do
    case "$1" in
        --gateway) gateway=$2; shift 2 ;;
        --code) code=$2; shift 2 ;;
        --workspace) workspace=$(CDPATH= cd -- "$2" && pwd); shift 2 ;;
        --home) builder_home=$(CDPATH= cd -- "$2" && pwd); shift 2 ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) printf 'Unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done
command -v systemctl >/dev/null 2>&1 || { echo 'systemd user services are required (Linux).' >&2; exit 1; }
builder_bin=$(command -v builder 2>/dev/null || true)
[ -n "$builder_bin" ] || { echo 'builder is not on PATH; run ./install.sh first.' >&2; exit 1; }
[ -n "$gateway" ] || { echo '--gateway is required.' >&2; exit 2; }
[ -n "$workspace" ] || { echo '--workspace is required.' >&2; exit 2; }
case "$gateway" in
    http://*|https://*) ;;
    *) echo '--gateway must be a complete http:// or https:// origin.' >&2; exit 2 ;;
esac
case "$gateway$workspace$builder_home" in
    *'
'*) echo 'Settings cannot contain newlines.' >&2; exit 2 ;;
esac

if [ -n "$code" ]; then
    if [ -n "$builder_home" ]; then
        BUILDER_HOME=$builder_home "$builder_bin" -C "$workspace" remote connect "$gateway" --code "$code" --pair-only
    else
        "$builder_bin" -C "$workspace" remote connect "$gateway" --code "$code" --pair-only
    fi
fi

quote() {
    escaped=$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')
    printf '"%s"' "$escaped"
}
mkdir -p "$config_dir" "$unit_dir"
temporary=$env_file.tmp.$$
trap 'rm -f "$temporary"' EXIT HUP INT TERM
umask 077
{
    printf 'BUILDER_REMOTE_GATEWAY='; quote "$gateway"; printf '\n'
    printf 'BUILDER_REMOTE_WORKSPACE='; quote "$workspace"; printf '\n'
    if [ -n "$builder_home" ]; then
        printf 'BUILDER_HOME='; quote "$builder_home"; printf '\n'
    fi
    if [ -f "$env_file" ]; then
        # Keep profile credentials, connector arguments, and user comments on
        # reinstall while replacing only values managed by this installer.
        awk '!/^BUILDER_REMOTE_GATEWAY=/ && !/^BUILDER_REMOTE_WORKSPACE=/ && !/^BUILDER_HOME=/' "$env_file"
    else
        printf '# Add model credential environment variables required by your profiles here.\n'
        printf '#BUILDER_BASIC_AUTH="Basic BASE64"\n'
        printf '#BUILDER_REMOTE_ARGS="--approval read-only"\n'
    fi
} > "$temporary"
mv "$temporary" "$env_file"
trap - EXIT HUP INT TERM
chmod 600 "$env_file"

builder_bin_sed=$(printf '%s' "$builder_bin" | sed 's/[&|\\]/\\&/g')
env_file_sed=$(printf '%s' "$env_file" | sed 's/[&|\\]/\\&/g')
sed -e "s|@BUILDER_BIN@|$builder_bin_sed|" -e "s|@ENV_FILE@|$env_file_sed|" "$here/builder-remote.service" > "$unit_dir/builder-remote.service"
systemctl --user daemon-reload
systemctl --user enable --now builder-remote
systemctl --user restart builder-remote
if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]; then
    printf 'The connector runs while you are logged in. To keep it active after logout:\n  loginctl enable-linger %s\n' "$USER"
fi
systemctl --user --no-pager --lines=0 status builder-remote || true
printf 'Builder will reconnect to %s automatically.\n' "$gateway"
