"""真实进程和四次真实 Cargo：整条升级准备不能被单命令总超时截断。"""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import threading
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
        toolchain=os.environ.get('EVE_TEST_UPGRADE_TOOLCHAIN')
        real_cargo=[cargo,*(['+'+toolchain] if toolchain else [])]
        (self.source/'crates/eve-app/src').mkdir(parents=True)
        (self.source/'Cargo.toml').write_text('[workspace]\nmembers=["crates/eve-app"]\nresolver="2"\n')
        (self.source/'Cargo.lock').write_text('version = 4\n\n[[package]]\nname = "eve-app"\nversion = "0.0.1"\n')
        (self.source/'crates/eve-app/Cargo.toml').write_text('[package]\nname="eve-app"\nversion="0.0.1"\nedition="2021"\n[[bin]]\nname="eve-qqbot"\npath="src/main.rs"\n')
        (self.source/'crates/eve-app/src/main.rs').write_text('fn main() {\n    println!("compiled candidate");\n}\n')
        (self.source/'AGENT.md').write_text('合成升级候选')
        (self.source/'scripts').mkdir()
        (self.source/'scripts/verify-self-upgrade.py').write_text("print('runtime contract fixture passed')\n")
        runtime_source=Path(__file__).resolve().parents[3]/'scripts'
        for name in ['build_resources.py','self_upgrade_runtime.py','source_update.py']:
            shutil.copy2(runtime_source/name,self.source/'scripts'/name)
        resources=self.source/'connectors/qqbot';resources.mkdir(parents=True)
        for file in ['bridge.mjs','bridge-core.mjs','package.json','package-lock.json']:(resources/file).write_text('{}')
        (self.source/'.gitignore').write_text('/target/\n')
        # 本例只测四次检查的累计期限。首次 rustup/组件装载和编译缓存不占每条检查剩余的两秒。
        # 与真实宿主的命令环境一致：只保留进程启动环境，不把模型/QQ 凭据交给构建。
        build_env={key:os.environ[key] for key in ('PATH','SystemRoot','TEMP','TMP') if key in os.environ}
        build_env['CARGO_TARGET_DIR']=str(self.source/'target')
        warmup=[]
        for arguments in [
            ['--version'],['fmt','--version'],['clippy','--version'],
            ['fmt','--','--check'],
            ['clippy','--release','--workspace','--all-targets','--locked','--','-D','warnings'],
            ['test','--release','--workspace','--locked'],
            ['build','-p','eve-app','--bins','--release','--locked'],
        ]:
            warmed_at=time.monotonic()
            result=subprocess.run([*real_cargo,*arguments],cwd=self.source,env=build_env,
                capture_output=True,text=True,timeout=30)
            record={'arguments':arguments,'elapsed_seconds':round(time.monotonic()-warmed_at,3),
                'exit_code':result.returncode}
            if '--version' in arguments:record['version']=result.stdout.strip()
            warmup.append(record)
            print(json.dumps({'upgrade_fixture_warmup':record,'toolchain':toolchain or 'host-default'},ensure_ascii=False),flush=True)
            self.assertEqual(result.returncode,0,'真实Cargo预热失败：'+result.stdout[-4000:]+result.stderr[-4000:])
        stages=self.work/'cargo-stages.log'
        wrapper=self.source/'cargo-wrapper.sh'
        # 事件放在源码目录外；记录真实 Cargo 耗时，不能让诊断本身改变待核验的源码指纹。
        stage_log=shlex.quote(str(stages))
        wrapper.write_text('#!/bin/sh\nexport CARGO_TARGET_DIR='+shlex.quote(str(self.source/'target'))+
            '\nprintf "start %s %s\\n" "$1" "$(date +%s.%N)" >> '+stage_log+
            '\ncase "$1" in fmt|clippy|test|build) sleep 11;; esac\n'+
            'printf "cargo %s %s\\n" "$1" "$(date +%s.%N)" >> '+stage_log+'\n'+
            shlex.join(real_cargo)+' "$@"\neve_fixture_exit=$?\n'+
            'printf "exit %s %s %s\\n" "$1" "$(date +%s.%N)" "$eve_fixture_exit" >> '+stage_log+
            '\nexit "$eve_fixture_exit"\n')
        wrapper.chmod(0o700)
        for args in [['init','-q'],['add','.'],['-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','fixture']]:
            subprocess.run(['git',*args],cwd=self.source,check=True,stdout=subprocess.DEVNULL)
        revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=self.source,text=True).strip()
        # 共享工作区残留另一会话的未完成改动；准备不得提交、复制或覆盖它们。
        shared_dirty='fn main() { unfinished_shared_change }\n'
        (self.source/'crates/eve-app/src/main.rs').write_text(shared_dirty)
        (self.source/'someone-unfinished.txt').write_text('保留其他会话',encoding='utf8')
        shared_index=(self.source/'.git/index').read_bytes()
        queue=self.work/'prepare-timeout-supervisor';queue.mkdir()
        heartbeat_stop=threading.Event()
        heartbeat_errors=[]
        def heartbeat():
            # 本夹具只提供准备阶段所需的在线基线，不申请切换、不伪造已上线结果。
            while not heartbeat_stop.is_set():
                try:
                    temporary=queue/'supervisor.tmp'
                    temporary.write_text(json.dumps({'format_version':1,'pid':os.getpid(),
                        'heartbeat_ms':int(time.time()*1000),'phase':'running','active_sha256':'a'*64,
                        'candidate_sha256':None,'result':None,'error':None,'restart_supported':True,
                        'active_source_revision':revision,'active_candidate_sha256':None,'host_nonce':'b'*64}))
                    temporary.replace(queue/'supervisor.json')
                except Exception as error:
                    heartbeat_errors.append(type(error).__name__)
                    return
                heartbeat_stop.wait(.2)
        heartbeat_thread=threading.Thread(target=heartbeat,daemon=True)
        heartbeat_thread.start()
        def stop_heartbeat():
            heartbeat_stop.set();heartbeat_thread.join(timeout=2)
        self.addCleanup(stop_heartbeat)
        config=json.loads(self.developer_config.read_text())
        config.update(command_timeout_ms=13_000,cargo_program=str(wrapper),live_upgrade=str(queue))
        self.developer_config.write_text(json.dumps(config))
        outcomes=[]
        verification_failures=[]
        reviews=[]
        def review(request):
            reviews.append(request)
            plan=json.loads(request['arguments'][0])
            expected_checks=[{'program':str(wrapper),'arguments':arguments,'cwd':'.'} for arguments in [
                ['fmt','--','--check'],
                ['clippy','--release','--workspace','--all-targets','--locked','--','-D','warnings'],
                ['test','--release','--workspace','--locked'],
                ['build','-p','eve-app','--bins','--release','--locked']]]
            allowed=(request['user_messages'][-1]=='准备新版本' and not request['delegated']
                and request['program']=='eve.prepare_self_upgrade'
                and plan['source']['head']==revision
                and plan['source_diff']['base_revision']==revision
                and plan['source_diff']['target_revision']==revision
                and plan['source_diff_content']=='' and plan['active_sha256']=='a'*64
                and plan['publish_only'] is True and plan['activate'] is False
                and plan['checks']==expected_checks
                and plan['after_rust_tests']=={'program':'python3','arguments':['-B','scripts/verify-self-upgrade.py'],'cwd':'.'})
            return {'risk':'medium','authorization':'high' if allowed else 'unknown',
                'outcome':'allow' if allowed else 'deny','rationale':'核对准备原话、完整空差异及四项真实Cargo和Python验收执行计划'}
        self.review_reply=review
        def respond(body):
            results=self.results(body)
            if not results:return tool_response([('prepare_self_upgrade',{})])
            self.assertEqual((self.source/'crates/eve-app/src/main.rs').read_text(),shared_dirty)
            self.assertEqual((self.source/'someone-unfinished.txt').read_text(encoding='utf8'),'保留其他会话')
            self.assertEqual((self.source/'.git/index').read_bytes(),shared_index)
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
                'host_prepare_timeout_ms':6*13_000+300_000,'process_timeout_seconds':90,
                'toolchain':toolchain or 'host-default','warmup':warmup,
                'cargo_stages':stages.read_text()[-8000:] if stages.exists() else '',
                'prepare_results':outcomes,'prepare_assertions':verification_failures,
                'provider_errors':self.errors,'bridge_replies':replies}
            raise AssertionError(str(error)+'\n升级准备真实现场：'+json.dumps(evidence,ensure_ascii=False)[-24000:]) from error
        self.assertGreater(time.monotonic()-start,43,'必须跨过旧的单命令 43 秒总期限')
        stop_heartbeat()
        self.assertEqual(heartbeat_errors,[])
        self.assertEqual(len(reviews),1,'准备候选必须独立审核一次')
        self.assertFalse((queue/'request.json').exists(),'准备不能申请上线')
        print(json.dumps({'upgrade_prepared':True,'elapsed_seconds':round(time.monotonic()-start,3),
            'command_timeout_ms':13_000,'toolchain':toolchain or 'host-default',
            'cargo_stages':stages.read_text()},ensure_ascii=False),flush=True)


if __name__=='__main__':unittest.main()
