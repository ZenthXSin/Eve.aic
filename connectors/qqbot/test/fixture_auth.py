"""旧业务验收使用的可信测试管理员；不修改生产默认权限或权限负例的显式配置。"""
import hashlib
import json
from pathlib import Path


def bootstrap_test_admin(state, environment, app="1904159860"):
    if environment.get("EVE_DEVELOPER_CONFIG") or environment.get("EVE_ADMIN_BIND_KEY"):
        return
    path = Path(state) / "admin-bindings.json"
    if path.exists():
        return
    subjects = [hashlib.sha256(json.dumps([app, scope, "user-1"], separators=(",", ":")).encode()).hexdigest()
                for scope in ["c2c", "group"]]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"version": 1, "subjects": subjects}), encoding="utf8")
