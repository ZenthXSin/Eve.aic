#!/usr/bin/env bash
set -euo pipefail
# Preparation is separate from service activation. Existing QQ and data remain running here.
if [[ $# != 3 || "$1" != --stage ]]; then
  echo 'usage: install.sh --stage /absolute/options.json /absolute/deno' >&2
  exit 2
fi
[[ "$2" = /* && "$3" = /* && $EUID -eq 0 ]]
script_directory=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for account in eve-exec eve-instance-a eve-instance-b; do
  if ! id "$account" >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$account"
  fi
done
command_uid=$(id -u eve-exec)
command_gid=$(id -g eve-exec)
instance_a_uid=$(id -u eve-instance-a)
instance_a_gid=$(id -g eve-instance-a)
instance_b_uid=$(id -u eve-instance-b)
instance_b_gid=$(id -g eve-instance-b)
# UID/GID in reviewed options must agree with the dedicated account; do not rewrite secrets/options.
python3 - "$2" "$command_uid" "$command_gid" "$instance_a_uid" "$instance_a_gid" "$instance_b_uid" "$instance_b_gid" <<'PY'
import json,sys
options=json.load(open(sys.argv[1]))
assert options['commandIdentity']=={'uid':int(sys.argv[2]),'gid':int(sys.argv[3])}, 'reviewed commandIdentity must match eve-exec'
assert options['instanceIdentities']==[{'uid':int(sys.argv[4]),'gid':int(sys.argv[5])},{'uid':int(sys.argv[6]),'gid':int(sys.argv[7])}], 'reviewed instanceIdentities must match eve-instance-a,b'
assert all(int(n)>0 for n in sys.argv[2:])
PY
exec "$3" run --allow-read --allow-write --allow-run --allow-env "$script_directory/main.ts" --stage "$2"
