#!/usr/bin/env bash
set -euo pipefail

# 只供远端固定镜像使用；不对开发机安装工具链或修改生产环境。
test "${GITHUB_ACTIONS:-}" = true
test "$(uname -m)" = x86_64
test -f /.dockerenv
. /etc/os-release
test "$ID" = debian
test "$VERSION_ID" = 12
[[ "${EVE_RELEASE_REVISION:-}" =~ ^[0-9a-f]{40}$ ]]
# checkout 的 safe.directory 仅写入它自己的临时 HOME；后续容器步骤需要持久配置。
# 仅信任当前显式检出的工作区，不能放宽为通配路径或改变宿主机配置。
release_workspace="$(realpath -e -- "${GITHUB_WORKSPACE:?}")"
test "$(pwd -P)" = "$release_workspace"
git config --global --add safe.directory "$release_workspace"
test "$(git rev-parse HEAD)" = "$EVE_RELEASE_REVISION"

apt-get -q update
apt-get install -y --no-install-recommends python3 bubblewrap binutils file curl xz-utils ca-certificates
node_archive="$EVE_RELEASE_OUTPUT/node-v22.22.0-linux-x64.tar.xz"
curl --fail --show-error --location --connect-timeout 20 --max-time 180 \
  --output "$node_archive" https://nodejs.org/dist/v22.22.0/node-v22.22.0-linux-x64.tar.xz
printf '%s  %s\n' 9aa8e9d2298ab68c600bd6fb86a6c13bce11a4eca1ba9b39d79fa021755d7c37 "$node_archive" | sha256sum --check --strict
tar -xJf "$node_archive" --strip-components=1 -C /usr/local
test "$(node --version)" = v22.22.0
# Eve 的受隔离 Node 执行器使用固定 /usr/bin/node。
test ! -e /usr/bin/node
ln -s /usr/local/bin/node /usr/bin/node
rm -- "$node_archive"
bwrap --unshare-all --ro-bind / / --dev /dev --proc /proc /usr/bin/true
{
  cat /etc/os-release
  rustc -Vv
  cargo -Vv
  node --version
  python3 --version
  getconf GNU_LIBC_VERSION
  dpkg-query -W bubblewrap binutils file python3 libc6
}
