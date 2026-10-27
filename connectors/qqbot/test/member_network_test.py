"""实际 QQ 成员命令不能访问宿主环回；默认隔离与可信启动器均验证。"""
import json,os,threading,unittest
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
import management_access_test as access
from dialogue_tools_test import text_response,tool_response
@unittest.skipUnless(os.name=="posix" and Path('/usr/bin/bwrap').is_file(),"requires bubblewrap")
class MemberNetworkAcceptance(access.ManagementAccessAcceptance):
 for _name in dir(access.ManagementAccessAcceptance):
  if _name.startswith("test_"):locals()[_name]=None
 del _name
 def test_command_cannot_reach_host_loopback(self):
  class Handler(BaseHTTPRequestHandler):
   def log_message(self,*a):pass
   def do_GET(self):self.send_response(200);self.end_headers();self.wfile.write(b"HOST_ONLY")
  server=ThreadingHTTPServer(("127.0.0.1",0),Handler)
  threading.Thread(target=server.serve_forever,daemon=True).start()
  self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
  script=f"import socket; s=socket.socket();s.settimeout(1);print(s.connect_ex(('127.0.0.1',{server.server_port})))"
  def respond(body):
   r=self.results(body)
   if not r:return tool_response([('run_command',{'program':'/usr/bin/python3','args':['-c',script]})])
   self.assertTrue(r[0]['success']);self.assertNotEqual(r[0]['stdout'].strip(),'0')
   return text_response('宿主环回隔离有效')
  self.callback=respond
  self.enabled(self.send(self.message('member-net','检查本机连接',user='user-2',scope='group',expected='宿主环回隔离有效')))
if __name__=='__main__':unittest.main()
