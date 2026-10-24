"""普通成员实际开发闭环：项目隔离、真实编译和 Mindustry 加载、交付与权限边界。"""
import json
import os
from pathlib import Path
import shlex
import sys
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response
from management_access_test import principal


SANDBOX = sys.platform == 'linux' and Path('/usr/bin/bwrap').is_file()
JAR = os.environ.get('EVE_MINDUSTRY_SERVER_JAR')


@unittest.skipUnless(SANDBOX, '成员命令需要 Linux bubblewrap')
class MemberDevelopmentAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def tools(self, operations):
        response = tool_response(operations)
        for call in response['choices'][0]['message']['tool_calls']:
            call['id'] += f'-{self.chat_rounds}'
        return response

    def run_member(self, script, real=False, **options):
        more = ['--practice-mindustry-server', JAR] if real else []
        return self.run_eve(script, interest=real, memory=True, research=False, practice=False,
            more=more, env_extra={'EVE_DEVELOPER_CONFIG':str(self.developer_config)}, **options)

    def member(self, id, text, expected='已核实', user='user-2'):
        return self.message(id, text, user=user, scope='group', target='group-1', expected=expected)

    def test_member_writes_edits_builds_and_cannot_read_host_or_other_member(self):
        marker = self.source / 'host-only.txt'
        marker.write_text('HOST_CONTENT_MUST_NOT_ESCAPE')
        def respond(body):
            results = self.results(body)
            current = next(m['content'] for m in reversed(body['messages']) if m['role']=='user')
            if current == '另一个成员读取':
                if not results:
                    return self.tools([('read_file', {'path':'project.txt'}), ('send_file', {'name':'build.txt'})])
                self.assertTrue(all('error' in r for r in results))
                return text_response('已隔离')
            if not results:
                return self.tools([('write_file', {'path':'project.txt','content':'draft','expected_sha256':''})])
            if len(results)==1:
                self.assertTrue(results[0]['success'])
                return self.tools([('edit_file', {'path':'project.txt','old_string':'draft','new_string':'compiled','expected_sha256':results[0]['sha256']})])
            if len(results)==2:
                return self.tools([('search_files', {'query':'compiled'}), ('read_file', {'path':'project.txt'})])
            if len(results)==4:
                self.assertEqual(results[2]['matches'][0]['path'],'project.txt')
                self.assertEqual(results[3]['content'],'compiled')
                command = 'test "$(cat project.txt)" = compiled && test ! -e '+shlex.quote(str(marker))+' && test -z "$EVE_OPENAI_API_KEY$QQBOT_APP_SECRET$EVE_WEB_TOKEN$EVE_GITHUB_TOKEN" && cp project.txt exports/build.txt && echo ISOLATION_VERIFIED'
                return self.tools([('run_command', {'program':'sh','args':['-c',command]})])
            self.assertTrue(results[-1]['success'], results[-1])
            self.assertIn('ISOLATION_VERIFIED', results[-1]['stdout'])
            self.assertNotIn('HOST_CONTENT_MUST_NOT_ESCAPE', json.dumps(results))
            return text_response('已核实')
        self.callback=respond
        self.run_member([*self.send(self.member('build','写入并构建')),
                         *self.send(self.member('foreign','另一个成员读取',expected='已隔离',user='user-3'))])
        self.assertFalse((self.source/'project.txt').exists())
        self.assertEqual(marker.read_text(),'HOST_CONTENT_MUST_NOT_ESCAPE')
        owned = self.source/'.eve-members'
        self.assertEqual(len(list(owned.glob('*/project.txt'))),1)

    def test_member_cannot_promote_work_to_deployment_or_escape_exports_by_symlink(self):
        (self.work/'state/qq-outbox/files').mkdir(parents=True)
        (self.work/'state/qq-outbox/files/host.txt').write_text('HOST_EXPORT_PRIVATE')
        def respond(body):
            results=self.results(body)
            if not results:
                return self.tools([('run_command',{'program':'sh','args':['-c','ln -s /etc/passwd exports/link.txt']}),
                                      ('prepare_self_upgrade',{}),('clear_all_contexts',{})])
            if len(results)==3:
                self.assertTrue(results[0]['success'],results[0])
                self.assertTrue(all('error' in r for r in results[1:]))
                return self.tools([('send_file',{'name':'link.txt'}),('send_file',{'name':'host.txt'}),('read_file',{'path':'../host-only.txt'})])
            self.assertTrue(all('error' in r for r in results[3:]))
            self.assertNotIn('HOST_EXPORT_PRIVATE',json.dumps(results))
            return text_response('边界已核实')
        self.callback=respond
        self.run_member(self.send(self.member('boundaries','检查开发边界',expected='边界已核实')))

    @unittest.skipUnless(JAR and Path(JAR).is_file(), '需要实际 Mindustry 无头服务端 JAR')
    def test_ordinary_member_compiles_java_mod_loads_real_mindustry_and_delivers_jar(self):
        source='''package eve.member;
import mindustry.mod.Mod;
import arc.util.Log;
public class MemberMod extends Mod { public void init(){Log.info("EVE_MEMBER_MOD_LOADED");} }
'''
        manifest='''name: eve-member-validation
displayName: Eve Member Validation
author: Eve
version: "1.0"
main: eve.member.MemberMod
java: true
minGameVersion: 146
'''
        verify='''import subprocess, time, selectors, os
p=subprocess.Popen(['java','-Xmx256m','-jar','/opt/eve/mindustry-server.jar'],cwd='server',stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,bufsize=0)
sel=selectors.DefaultSelector();sel.register(p.stdout,selectors.EVENT_READ)
seen=False;deadline=time.monotonic()+25
try:
 while time.monotonic()<deadline:
  events=sel.select(0.2)
  if events:
   line=os.read(p.stdout.fileno(),4096).decode('utf8',errors='replace')
   if not line:break
   print(line,end='',flush=True)
   if 'EVE_MEMBER_MOD_LOADED' in line:
    seen=True;p.stdin.write(b'mods\\nexit\\n');p.stdin.flush();break
 assert seen,'mod init was not observed in real game runtime'
 p.wait(timeout=8)
 assert p.returncode==0,p.returncode
 print('REAL_MINDUSTRY_MOD_VERIFIED',flush=True)
finally:
 if p.poll() is None:p.kill();p.wait()
'''
        def respond(body):
            results=self.results(body)
            if not results:
                return self.tools([('run_command',{'program':'sh','args':['-c','mkdir -p src/eve/member']})])
            if len(results)==1:
                self.assertTrue(results[0]['success'],results[0])
                return self.tools([('write_file',{'path':path,'content':content,'expected_sha256':''}) for path,content in
                    [('src/eve/member/MemberMod.java',source),('mod.hjson',manifest),('verify.py',verify)]])
            if len(results)==4:
                self.assertTrue(all(r['success'] for r in results))
                command='mkdir -p classes server/config/mods && javac -cp /opt/eve/mindustry-server.jar -d classes src/eve/member/MemberMod.java && jar --create --file exports/member-mod.jar -C classes . -C . mod.hjson && cp exports/member-mod.jar server/config/mods/ && python3 verify.py'
                return self.tools([('run_command',{'program':'sh','args':['-c',command],'timeout_ms':45000})])
            if len(results)==5:
                self.assertTrue(results[-1]['success'],results[-1])
                self.assertIn('REAL_MINDUSTRY_MOD_VERIFIED',results[-1]['stdout'])
                return self.tools([('send_file',{'name':'member-mod.jar'})])
            self.assertTrue(results[-1]['queued'])
            return text_response('编译及真实游戏加载验证通过')
        self.callback=respond
        def delivered():
            self.wait_for(lambda:any(r['state']=='Sent' for r in json.loads((self.work/'state/qq-outbox/ledger.json').read_text())['records']), '模组文件未收到发送回执',10)
        self.run_member([*self.send(self.member('mod','制作并验证模组',expected='编译及真实游戏加载验证通过')),
                         *self.checkpoint('file-sent')],real=True,checkpoints={'file-sent':delivered},timeout=65)
        ledger=json.loads((self.work/'state/qq-outbox/ledger.json').read_text())
        self.assertEqual(ledger['records'][0]['state'],'Sent')
        self.assertEqual(ledger['records'][0]['name'],'member-mod.jar')
        self.assertFalse((self.source/'src/eve/member/MemberMod.java').exists())
        events=[json.loads(line) for line in (self.work/f'events-{self.runs}.jsonl').read_text().splitlines()]
        files=[event for event in events if event.get('direction')=='out' and event.get('type')=='file']
        self.assertEqual(len(files),1)
        self.assertEqual((files[0]['scope'],files[0]['target_id'],files[0]['name']),('group','group-1','member-mod.jar'))


if __name__=='__main__':unittest.main()
