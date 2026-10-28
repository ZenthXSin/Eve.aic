"""真实进程和四次真实 Cargo：整条升级准备不能被单命令总超时截断。"""
import json
import os
import shlex
import shutil
import subprocess
import time
import unittest
import developer_tools_test as developer
from dialogue_tools_test import text_response,tool_response


@unittest.skipUnless(os.name=='posix','POSIX cargo wrapper')
class UpgradeTimeoutAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):locals()[_name]=None
    del _name

    def test_four_real_validations_complete_beyond_old_single_command_timeout(self):
        cargo=shutil.which('cargo')
        self.assertIsNotNone(cargo)
        (self.source/'crates/eve-app/src').mkdir(parents=True)
        (self.source/'Cargo.toml').write_text('[workspace]\nmembers=["crates/eve-app"]\nresolver="2"\n')
        (self.source/'Cargo.lock').write_text('version = 4\n\n[[package]]\nname = "eve-app"\nversion = "0.0.1"\n')
        (self.source/'crates/eve-app/Cargo.toml').write_text('[package]\nname="eve-app"\nversion="0.0.1"\nedition="2021"\n[[bin]]\nname="eve-qqbot"\npath="src/main.rs"\n')
        (self.source/'crates/eve-app/src/main.rs').write_text('fn main() {\n    println!("compiled candidate");\n}\n')
        (self.source/'AGENT.md').write_text('合成升级候选')
        resources=self.source/'connectors/qqbot';resources.mkdir(parents=True)
        for file in ['bridge.mjs','bridge-core.mjs','package.json','package-lock.json']:(resources/file).write_text('{}')
        (self.source/'.gitignore').write_text('/target/\n')
        stages=self.work/'cargo-stages.log'
        wrapper=self.source/'cargo-wrapper.sh'
        # 事件放在源码目录外；记录真实 Cargo 耗时，不能让诊断本身改变待核验的源码指纹。
        stage_log=shlex.quote(str(stages))
        wrapper.write_text('#!/bin/sh\nexport CARGO_TARGET_DIR='+shlex.quote(str(self.source/'target'))+
            '\nprintf "start %s %s\\n" "$1" "$(date +%s.%N)" >> '+stage_log+
            '\ncase "$1" in fmt|clippy|test|build) sleep 11;; esac\n'+
            'printf "cargo %s %s\\n" "$1" "$(date +%s.%N)" >> '+stage_log+'\n'+
            shlex.quote(cargo)+' "$@"\neve_fixture_exit=$?\n'+
            'printf "exit %s %s %s\\n" "$1" "$(date +%s.%N)" "$eve_fixture_exit" >> '+stage_log+
            '\nexit "$eve_fixture_exit"\n')
        wrapper.chmod(0o700)
        for args in [['init','-q'],['add','.'],['-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','fixture']]:
            subprocess.run(['git',*args],cwd=self.source,check=True,stdout=subprocess.DEVNULL)
        config=json.loads(self.developer_config.read_text())
        config.update(command_timeout_ms=13_000,cargo_program=str(wrapper))
        self.developer_config.write_text(json.dumps(config))
        outcomes=[]
        verification_failures=[]
        def respond(body):
            results=self.results(body)
            if not results:return tool_response([('prepare_self_upgrade',{})])
            result=results[-1]
            outcomes.append(result)
            try:
                self.assertTrue(result.get('prepared'),result)
                self.assertEqual(len(result['checks']),4)
                self.assertTrue(all(c['success'] for c in result['checks']))
                self.assertFalse(result['applied'])
            except AssertionError as error:
                # 失败结果照常触发严格回复校验；不把测试断言变成 HTTP 500 引起模型重试。
                verification_failures.append(str(error))
                return text_response('候选准备校验失败，详情见验收记录')
            return text_response('四项检查完成，候选已准备')
        self.callback=respond
        start=time.monotonic()
        try:
            self.run_eve(self.send(self.message('prepare','准备新版本',expected='四项检查完成，候选已准备')),
                interest=False,memory=True,research=False,practice=False,timeout=90,
                env_extra={'EVE_DEVELOPER_CONFIG':str(self.developer_config)})
        except (AssertionError,subprocess.TimeoutExpired) as error:
            # HTTP 回调内的断言会先转成 500，桥接可能只留下 wrong_reply_text；保留原始检查结果。
            events=self.work/f'events-{self.runs}.jsonl'
            replies=[json.loads(line) for line in events.read_text().splitlines()
                     if json.loads(line).get('type')=='reply'] if events.exists() else []
            evidence={'elapsed_seconds':round(time.monotonic()-start,3),'command_timeout_ms':13_000,
                'host_prepare_timeout_ms':4*13_000+240_000,'process_timeout_seconds':90,
                'cargo_stages':stages.read_text()[-8000:] if stages.exists() else '',
                'prepare_results':outcomes,'prepare_assertions':verification_failures,
                'provider_errors':self.errors,'bridge_replies':replies}
            raise AssertionError(str(error)+'\n升级准备真实现场：'+json.dumps(evidence,ensure_ascii=False)[-24000:]) from error
        self.assertGreater(time.monotonic()-start,43,'必须跨过旧的单命令 43 秒总期限')
        print(json.dumps({'upgrade_prepared':True,'elapsed_seconds':round(time.monotonic()-start,3),
            'command_timeout_ms':13_000,'cargo_stages':stages.read_text()},ensure_ascii=False),flush=True)


if __name__=='__main__':unittest.main()
