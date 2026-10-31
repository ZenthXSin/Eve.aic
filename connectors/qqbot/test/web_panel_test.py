"""真实 Eve/HTTP 面板与本地模型、QQ 替身；只停止本测试持有的子进程。"""
from fixture_auth import configure_fixture_progress
import contextlib
import http.client
import json
import os
import pathlib
import queue
import signal
import subprocess
import threading
import time
import unittest
import urllib.error
import urllib.request

import eve_e2e as support

TOKEN = "synthetic-web-panel-token-12345678901234567890"
BRIDGE = pathlib.Path(__file__).with_name("web-panel-bridge.mjs")


class WebPanelAcceptance(unittest.TestCase):
    setUp = support.Acceptance.setUp
    tearDown = support.Acceptance.tearDown
    message = support.Acceptance.message
    gate = support.Acceptance.gate
    documents = support.Acceptance.documents
    request_arrived = support.Acceptance.request_arrived
    sessions = support.Acceptance.sessions
    assert_cancelled = support.Acceptance.assert_cancelled

    def api(self, url, path, body=None, token=TOKEN):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(url + path, data=data, headers={
            "Authorization": "Bearer " + token, "Content-Type": "application/json"})
        try:
            response = urllib.request.urlopen(request, timeout=6)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, json.loads(response.read())

    def until(self, predicate, child, timeout=12):
        deadline = time.monotonic() + timeout
        while child.poll() is None and time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            time.sleep(0.01)
        self.fail("owned Eve process did not reach the synchronization point")

    @contextlib.contextmanager
    def launch(self, script, token=TOKEN):
        self.runs += 1
        scenario = self.work / f"web-scenario-{self.runs}.json"
        events = self.work / f"web-events-{self.runs}.jsonl"
        errors = self.work / f"web-errors-{self.runs}.txt"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events), "error_file": str(errors)}))
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}", EVE_OPENAI_PROTOCOL="chat",
                   EVE_LLM_RESPONSE_MODE="complete", EVE_WEB_TOKEN=token)
        command = [str(support.BINARY), "--state-dir", str(self.work / "state"), "--agent", str(support.ROOT / "AGENT.md"),
                   "--bridge-script", str(BRIDGE), "--bridge-arg", str(scenario), "--web-listen", "127.0.0.1:0"]
        from fixture_auth import bootstrap_test_admin, bootstrap_fixture_groups
        bootstrap_test_admin(self.work / "state", env)
        bootstrap_fixture_groups(self.work / "state", scenario)
        child = subprocess.Popen(command, env=configure_fixture_progress(env), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        lines = []
        ready = queue.Queue()
        def stderr_reader():
            for line in child.stderr:
                lines.append(line)
                if line.startswith("EVE_WEB_READY "):
                    ready.put(line.split(None, 1)[1].strip())
        reader = threading.Thread(target=stderr_reader, daemon=True)
        reader.start()
        try:
            url = ready.get(timeout=12)
            yield child, url
            child.wait(timeout=15)
            stdout = child.stdout.read()
            reader.join(timeout=3)
            self.assertEqual(child.returncode, 0, "".join(lines))
            summary = json.loads(stdout)
            self.assertTrue(summary["closed"])
            self.assertFalse(summary["terminal_error"])
            self.assertFalse(errors.exists(), errors.read_text() if errors.exists() else "")
            self.assertFalse(self.server_errors, "\n".join(self.server_errors))
            self.events = [json.loads(line) for line in events.read_text().splitlines()]
            for secret in (TOKEN, token, "test-model-secret", "test-app-secret"):
                self.assertNotIn(secret, stdout + "".join(lines))
            self.assertNotIn(token, (self.work / "state/state.json").read_text())
            with self.assertRaises((urllib.error.URLError, TimeoutError, ConnectionError, http.client.HTTPException)):
                urllib.request.urlopen(url + "/", timeout=1)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
            child.stdout.close()
            reader.join(timeout=3)
            child.stderr.close()

    def hold(self, name="end"):
        return {"wait_file": str(self.gate(name))}

    def test_context_notes_preview_cas_history_and_restart_do_not_change_original_records_or_call_model(self):
        original = '原始用户消息不能被便笺编辑覆盖'
        note = '<img src=x onerror="window.eveXss=true"> 模组已编译，尚未验证实际运行'
        changed = '产物读回通过；仍未发布'
        first = self.message('note-seed', original)
        next_message = self.message('note-read', '检查下一轮的便笺')
        script = [{'send':first}, {'wait_command':{'id':'note-seed','type':'reply'}}, self.hold('notes-edit'),
                  {'send':next_message}, {'wait_command':{'id':'note-read','type':'reply'}}, self.hold('notes-end')]
        with self.launch(script) as (child, url):
            def ready():
                code, sessions = self.api(url,'/api/sessions?limit=25')
                if code != 200 or not sessions['sessions']:
                    return None
                key = sessions['sessions'][0]['key']
                code, detail = self.api(url,'/api/session',{'key':key,'limit':25})
                return key if code == 200 and detail['session']['turns'][-1]['status']['state'] == 'Completed' else None
            key = self.until(ready, child)
            original_sessions = self.documents()['eve.session']
            requests_before = len(self.requests)
            def action(operation, **values):
                return self.api(url,'/api/context-notes',{'key':key,'action':{'operation':operation,**values}})
            self.assertEqual(action('inspect')[1]['view']['revision'],0)
            self.assertNotIn('eve.context-notes',self.documents())
            code, preview = action('preview',text=note)
            self.assertEqual(code,200)
            self.assertFalse(preview['saved'])
            self.assertIn(note.replace('"','\\"'),preview['preview']['rendered'])
            self.assertNotIn('eve.context-notes',self.documents())
            self.assertEqual(action('update',expected_revision=0,text=note)[1]['view']['revision'],1)
            self.assertEqual(action('update',expected_revision=1,text=changed)[1]['view']['revision'],2)
            self.assertEqual(action('update',expected_revision=1,text='过时覆盖')[0],409)
            self.assertEqual(action('inspect')[1]['view']['text'],changed)
            code, versions = action('versions',limit=1)
            self.assertEqual(code,200)
            self.assertEqual([v['revision'] for v in versions['versions']],[2])
            self.assertEqual(versions['next_before'],2)
            self.assertEqual(action('versions',before=2,limit=1)[1]['versions'][0]['revision'],1)
            self.assertEqual(action('read_version',revision=1)[1]['view']['text'],note)
            self.assertEqual(action('rollback',expected_revision=2,target_revision=1)[1]['view']['revision'],3)
            self.assertEqual(action('inspect')[1]['view']['text'],note)
            foreign = {**key,'user_id':'other-user'}
            self.assertEqual(self.api(url,'/api/context-notes',{'key':foreign,'action':{'operation':'inspect'}})[0],404)
            self.assertEqual(self.api(url,'/api/context-notes',{'key':key,'channel':'console','action':{'operation':'inspect'}})[0],400)
            self.assertEqual(self.api(url,'/api/context-notes',{'key':key,'action':{'operation':'inspect'}},token='incorrect-token')[0],401)
            self.assertEqual(len(self.requests),requests_before,'HTTP 管理操作不请求模型')
            self.assertEqual(self.documents()['eve.session'],original_sessions,'便笺写入不改写原话或已发生的工具')
            self.gate('notes-edit').touch()
            self.until(lambda: len(self.requests) >= requests_before+1,child)
            self.assertIn('eve-context-note-v1',json.dumps(self.requests[-1],ensure_ascii=False))
            self.assertIn('模组已编译',json.dumps(self.requests[-1],ensure_ascii=False))
            self.gate('notes-end').touch()
        records = self.documents()['eve.context-notes']['index.v1']['records']
        self.assertEqual(len(records),1)
        self.assertEqual(len(next(iter(records.values()))['versions']),3)
        self.assertEqual(len(list((self.work/'state/context-notes-archive').glob('*.json'))),2)
        restart = self.message('note-restart','重启后继续核对便笺')
        with self.launch([{'send':restart},{'wait_command':{'id':'note-restart','type':'reply'}},self.hold('notes-restart')]) as (child,url):
            self.until(lambda: len(self.requests) >= requests_before+2,child)
            code, current = self.api(url,'/api/context-notes',{'key':key,'action':{'operation':'inspect'}})
            self.assertEqual(code,200)
            self.assertEqual((current['view']['revision'],current['view']['text']),(3,note))
            self.assertIn('模组已编译',json.dumps(self.requests[-1],ensure_ascii=False))
            self.gate('notes-restart').touch()

    def test_panel_reads_cancels_exact_generation_and_recovers_without_replay(self):
        self.provider_steps[1] = {"wait": "old-never"}
        original = self.message("web-active", "待取消的真实模型请求", expected_type="finish")
        script = [{"send": original}, {"wait_file": str(self.request_arrived(1))},
                  {"wait_command": {"id": "web-active", "type": "finish"}}, self.hold()]
        with self.launch(script) as (child, url):
            self.until(lambda: self.request_arrived(1).exists(), child)
            code, status = self.api(url, "/api/status")
            self.assertEqual(code, 200)
            self.assertTrue(status["qq"]["ready"])
            # 面板预装记忆和认知以支持兴趣热启用；无兴趣参数仍不请求观察模型。
            code, goals = self.api(url, "/api/goals")
            self.assertEqual(code, 200)
            self.assertEqual(goals["items"], [])
            self.assertEqual(self.api(url, "/api/goal?id=any-goal"), (404, {"error": "not_found"}))
            scope = {"channel": "qq", "session_id": "any-session", "user_id": "any-user"}
            self.assertEqual(self.api(url, "/api/memory/scopes", {})[0], 200)
            for path, body in [("/api/memory/scope", {"scope": scope}),
                               ("/api/memory/evidence", {"scope": scope, "id": "any-evidence"})]:
                self.assertEqual(self.api(url, path, body), (404, {"error": "not_found"}))
            code, sessions = self.api(url, "/api/sessions?limit=1")
            self.assertEqual(code, 200)
            key = sessions["sessions"][0]["key"]
            _, tasks = self.api(url, "/api/tasks")
            target = tasks["tasks"][0]["key"]
            self.assertEqual(target["session"], key)
            _, detail = self.api(url, "/api/session", {"key": key, "limit": 1})
            self.assertEqual(detail["session"]["turns"][0]["status"]["state"], "Pending")
            stale = {**target, "generation": target["generation"] + 1}
            self.assertEqual(self.api(url, "/api/cancel", {"target": stale})[0], 409)
            self.assertEqual(len(self.requests), 1)
            self.assertEqual(self.api(url, "/api/cancel", {"target": target}), (202, {"status": "requested"}))
            def cancelled():
                _, value = self.api(url, "/api/session", {"key": key})
                return value if value["session"]["turns"][0]["status"]["state"] == "Failed" else None
            detail = self.until(cancelled, child)
            self.assertEqual(detail["session"]["turns"][0]["status"]["code"], "Cancelled")
            self.assertEqual(detail["control"]["commit"], "Failed")
            self.gate("end").touch()
        self.assert_cancelled(self.sessions()[0]["turns"][0])
        self.assertFalse([event for event in self.events if event.get("type") == "reply"])
        restarted_script = [{"send": original}, {"wait_command": {"id": "web-active", "type": "finish"}},
                            {"send": self.message("fresh", "恢复后新任务")},
                            {"wait_receipt": {"path": str(self.work / "state/state.json"), "id": "fresh", "state": "Sent"}}, self.hold("restart-end")]
        with self.launch(restarted_script, token=TOKEN + "-new") as (child, url):
            self.until(lambda: self.request_arrived(2).exists(), child)
            self.assertEqual(self.api(url, "/api/status")[0], 401)
            _, detail = self.api(url, "/api/session", {"key": key, "limit": 1}, token=TOKEN + "-new")
            self.assertEqual(detail["session"]["turns"][0]["id"], 2)
            self.assertEqual(detail["session"]["next_before"], 2)
            _, older = self.api(url, "/api/session", {"key": key, "limit": 1, "before": 2}, token=TOKEN + "-new")
            self.assertEqual(older["session"]["turns"][0]["status"]["code"], "Cancelled")
            self.assertIsNone(older["session"]["next_before"])
            self.assertEqual(self.api(url, "/api/cancel", {"target": target}, token=TOKEN + "-new")[0], 409)
            self.gate("restart-end").touch()
        self.assertEqual(len(self.requests), 2)

    def test_sigterm_closes_panel_and_settles_only_owned_qq_process(self):
        self.provider_steps[1] = {"wait": "old-never"}
        with self.launch([{"send": self.message("active", "请求", expected_type="finish")}, self.hold()]) as (child, url):
            self.until(lambda: self.request_arrived(1).exists(), child)
            self.assertEqual(self.api(url, "/api/status")[0], 200)
            child.send_signal(signal.SIGTERM)
        self.assert_cancelled(self.sessions()[0]["turns"][0])
        self.assertEqual(len(self.requests), 1)


if __name__ == "__main__":
    unittest.main()
