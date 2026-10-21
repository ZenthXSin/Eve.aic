#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
if [ -n "${HTTPS_PROXY:-${https_proxy:-}}" ]; then
    export NODE_USE_ENV_PROXY=1
    set -- --require="$ROOT/runtime/proxy/qq-proxy.cjs" "$@"
fi
exec "$ROOT/runtime/lib/ld-linux-x86-64.so.2" --library-path "$ROOT/runtime/lib" \
    "$ROOT/runtime/node/bin/node" --require="$ROOT/runtime/node-path.cjs" "$@"
