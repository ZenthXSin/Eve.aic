"""本地模型与 QQ 验收的公共默认、进度分类和可信测试管理员；不修改生产权限。"""
from contextlib import closing
import hashlib
import json
import sqlite3
from pathlib import Path



def is_progress_request(body):
    """只识别宿主进度系统提示和完整事实形状，用户正文相似 JSON 不会冒充。"""
    messages = body.get("messages", [])
    if len(messages) != 2 or messages[0].get("role") != "system" or messages[1].get("role") != "user":
        return False
    if not str(messages[0].get("content", "")).startswith("根据下一条JSON中的真实事件，自由写一句自然的中文任务进展"):
        return False
    try:
        facts = json.loads(messages[1]["content"])
    except (ValueError, TypeError, KeyError):
        return False
    return isinstance(facts, dict) and {"user_request", "phase", "tool_names", "failed_calls", "waiting_seconds", "previous_messages", "group"} <= facts.keys()


def progress_fixture_text(facts):
    if facts.get("phase") == "compaction":
        return "正在整理已确认的上下文，完成后继续核验。"
    if facts.get("phase") == "waiting":
        return "这一步仍在等待结果，拿到后继续核对。"
    if facts.get("phase") == "start":
        return "先核对已有资料，再推进当前任务。"
    return "已收到阶段回执，接下来继续核对结果。"

def configure_fixture_progress(environment):
    """普通验收关闭阶段、慢等待与工具里程碑；专测在自身环境中显式开启。"""
    # 通用场景验证业务不重放；传输重试由 model_retry_test 等显式开启验证。
    environment.setdefault("EVE_OPENAI_TRANSIENT_RETRIES", "false")
    # 账本格式验收单独开启；其余夹具继续检查旧格式的业务字段。
    environment.setdefault("EVE_SESSION_COLD_ARCHIVE", "false")
    environment.setdefault("EVE_PROGRESS_STAGE_UPDATES", "false")
    environment.setdefault("EVE_PROGRESS_SLOW_AFTER_MS", "0")
    environment.setdefault("EVE_PROGRESS_TOOL_MILESTONES", "false")
    return environment


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


def bootstrap_fixture_groups(state, scenario):
    """仅为隔离场景声明的群建立白名单，不改变生产策略和身份。"""
    groups = set()
    def visit(value):
        if isinstance(value, dict):
            group = value.get("target_id")
            if value.get("scope") == "group" and isinstance(group, str) and group.isascii() and 0 < len(group) <= 128 and all(c.isalnum() or c in "_-" for c in group):
                groups.add(group)
            for child in value.values():
                visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)
    visit(json.loads(Path(scenario).read_text()))
    bootstrap_fixture_group_ids(state, groups)


def bootstrap_fixture_group_ids(state, groups):
    """真实 Node 桥接夹具显式声明测试群；拒绝非法标识，不放宽生产白名单。"""
    groups = set(groups)
    if any(not isinstance(g, str) or not g.isascii() or not 0 < len(g) <= 128
           or not all(c.isalnum() or c in "_-" for c in g) for g in groups):
        raise ValueError("invalid_fixture_group_id")
    if not groups:
        return
    state = Path(state)
    state.mkdir(parents=True, exist_ok=True)
    # sqlite3 的上下文只提交/回滚，不负责关闭；Windows 删除夹具前必须释放句柄。
    with closing(sqlite3.connect(state / "qq-group-whitelist.sqlite3")) as connection:
        with connection:
            connection.execute("CREATE TABLE IF NOT EXISTS groups (group_id TEXT PRIMARY KEY, added_at INTEGER NOT NULL)")
            connection.executemany("INSERT OR IGNORE INTO groups VALUES (?, 0)", [(g,) for g in sorted(groups)])
            connection.execute("PRAGMA user_version=1")


def expand_learning_archive(documents, directory):
    """验收读取真实冷文件并校验内容摘要，保持旧夹具的逻辑文档视图。"""
    learning = documents.get("eve.learning", {})
    for key, kind, field in [("learning.v1", "jobs", "jobs"), ("learning.decisions.v1", "decisions", "records")]:
        index = learning.get(key)
        if not index or index.get("format_version") != 2:
            continue
        assert index["kind"] == kind
        records = []
        for ref in index["records"]:
            digest = ref["hash"]
            assert len(digest) == 64 and all(c in "0123456789abcdef" for c in digest)
            payload = (Path(directory) / (digest + ".json")).read_bytes()
            assert len(payload) == ref["bytes"] and hashlib.sha256(payload).hexdigest() == digest
            records.append(json.loads(payload))
        learning[key] = {"format_version": 1, field: records}
    return documents
