"""旧业务验收使用的可信测试管理员；不修改生产默认权限或权限负例的显式配置。"""
import hashlib
import json
from pathlib import Path


def bootstrap_test_admin(state, environment, app="1904159860", users=("user-1", "user-2")):
    if environment.get("EVE_DEVELOPER_CONFIG") or environment.get("EVE_ADMIN_BIND_KEY"):
        return
    path = Path(state) / "admin-bindings.json"
    existing = json.loads(path.read_text()) if path.exists() else {"version": 1, "subjects": []}
    subjects = [hashlib.sha256(json.dumps([app, scope, user], separators=(",", ":")).encode()).hexdigest()
                for scope in ["c2c", "group"] for user in users]
    path.parent.mkdir(parents=True, exist_ok=True)
    merged = sorted(set(existing["subjects"]) | set(subjects))
    if merged != sorted(existing["subjects"]):
        path.write_text(json.dumps({"version": 1, "subjects": merged}), encoding="utf8")
