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
        wrapper=self.source/'cargo-wrapper.sh'
        wrapper.write_text('#!/bin/sh\nexport CARGO_TARGET_DIR='+shlex.quote(str(self.source/'target'))+'\ncase "$1" in fmt|clippy|test|build) sleep 11;; esac\nexec '+shlex.quote(cargo)+' "$@"\n')
        wrapper.chmod(0o700)
        for args in [['init','-q'],['add','.'],['-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','fixture']]:
            subprocess.run(['git',*args],cwd=self.source,check=True,stdout=subprocess.DEVNULL)
        config=json.loads(self.developer_config.read_text())
        config.update(command_timeout_ms=13_000,cargo_program=str(wrapper))
        self.developer_config.write_text(json.dumps(config))
        def respond(body):
            results=self.results(body)
            if not results:return tool_response([('prepare_self_upgrade',{})])
            result=results[-1]
            self.assertTrue(result.get('prepared'),result)
            self.assertEqual(len(result['checks']),4)
            self.assertTrue(all(c['success'] for c in result['checks']))
            self.assertFalse(result['applied'])
            return text_response('四项检查完成，候选已准备')
        self.callback=respond
        start=time.monotonic()
        self.run_eve(self.send(self.message('prepare','准备新版本',expected='四项检查完成，候选已准备')),
            interest=False,memory=True,research=False,practice=False,timeout=90,
            env_extra={'EVE_DEVELOPER_CONFIG':str(self.developer_config)})
        self.assertGreater(time.monotonic()-start,43,'必须跨过旧的单命令 43 秒总期限')


if __name__=='__main__':unittest.main()
