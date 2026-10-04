"""仅停止当前仓库 main 上显式指定的 QQ 训练运行，不修改源码或其他任务。"""
import json
import os
from pathlib import Path
import time
import urllib.error
import urllib.request

WORKFLOW = ".github/workflows/qqbot-training.yml"


def replace_run(request, run_id, current_run_id, timeout=180, sleep=time.sleep, clock=time.monotonic):
    if type(run_id) is not int or run_id <= 0 or run_id == current_run_id:
        raise ValueError("invalid_replacement_run")
    path = f"/actions/runs/{run_id}"
    run = request("GET", path)
    if run.get("path") != WORKFLOW or run.get("head_branch") != "main":
        raise ValueError("replacement_must_be_main_qq_training")
    if run.get("status") == "completed":
        return {"run_id": run_id, "status": "already_completed"}
    # 停止指令是用户切换环境的明确请求。旧 runner 尚未上传的数据
    # 不能从新的 runner 导出；保留已有 artifacts，不删除既有训练结果。
    request("POST", path + "/cancel")
    deadline = clock() + timeout
    while clock() < deadline:
        run = request("GET", path)
        if run.get("status") == "completed":
            return {"run_id": run_id, "status": "completed", "conclusion": run.get("conclusion")}
        sleep(5)
    raise ValueError("previous_run_not_stopped_no_new_bot_started")


def main():
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise SystemExit("只允许在 main 停止旧训练运行。")
    if os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch":
        raw = os.environ.get("INPUT_REPLACE_RUN_ID", "").strip()
        run_id = int(raw) if raw else None
    else:
        run_id = json.loads(Path(".github/qqbot-training-request.json").read_text()).get("replace_run_id")
    if run_id is None:
        print("没有显式替换目标；训练任务沿用同机器人串行准入。")
        return
    repo = os.environ["GITHUB_REPOSITORY"]
    token = os.environ["GITHUB_TOKEN"]
    def request(method, path):
        req = urllib.request.Request("https://api.github.com/repos/" + repo + path,
            data=b"{}" if method == "POST" else None, method=method,
            headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"})
        with urllib.request.urlopen(req, timeout=20) as response:
            data = response.read()
            return json.loads(data) if data else {}
    try:
        result = replace_run(request, run_id, int(os.environ["GITHUB_RUN_ID"]))
    except (ValueError, OSError, urllib.error.HTTPError):
        raise SystemExit("旧训练未确认停止；拒绝启动第二个机器人，请检查 Actions 权限及目标运行。") from None
    print(json.dumps(result))
    print("旧运行已结束，已有 artifacts 保留；未上传的旧 runner 临时记录无法保证恢复。")


if __name__ == "__main__":
    main()
