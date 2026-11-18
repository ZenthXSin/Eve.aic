#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
image=${STUDIO_BROWSER_IMAGE:-remote-dom-engine:0.9.16-quiet-security-20261023}
deno=${DENO_BIN:-$(command -v deno || printf '%s' "$HOME/.local/share/eve-rebuild-tools/deno")}
[[ -x $deno ]] || { echo 'Need a local Deno binary (DENO_BIN).' >&2; exit 2; }
docker image inspect "$image" --format 'Offline Studio browser image: {{.Id}}' >&2
name="studio-browser-$(cat /proc/sys/kernel/random/uuid)"
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || :; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

tar -cf - contracts kernel sdk plugins/eve services/admin/{contracts,server,live,workspace,tool-catalog}.ts \
  services/admin/studio* tests/studio_browser* |
  docker run --rm --init -i --name "$name" --pull never --network none --read-only \
    --user 1000:1000 --cap-drop ALL --security-opt no-new-privileges --pids-limit 384 \
    --shm-size 384m --tmpfs /tmp:rw,nosuid,nodev,size=512m,mode=1777 \
    --mount "type=bind,src=$(realpath "$deno"),dst=/tools/deno,readonly" \
    -e HOME=/tmp/home -e DENO_DIR=/tmp/deno -e DENO_NO_UPDATE_CHECK=1 \
    --entrypoint /bin/sh "$image" -ec '
      umask 077; mkdir -p /tmp/src "$HOME"; cd /tmp/src
      tar --no-same-owner -xf -
      for screen in 91 92; do
        Xvfb ":$screen" -screen 0 1680x1050x24 -nolisten tcp >/tmp/x-$screen.log 2>&1 &
      done
      exec timeout -k 5 300 /tools/deno run --no-config --no-lock --cached-only -A \
        tests/studio_browser.mjs "$@"
    ' studio "$@"
