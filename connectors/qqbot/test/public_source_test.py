"""真实 QQ 宿主拒绝通过网页工具读取宿主环回服务。"""
import json
import threading
import unittest
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import management_access_test as access
from dialogue_tools_test import text_response,tool_response
class PublicSourceAcceptance(access.ManagementAccessAcceptance):
 for _name in dir(access.ManagementAccessAcceptance):
  if _name.startswith("test_"):locals()[_name]=None
 del _name
 def test_member_cannot_read_host_loopback_via_source_tool(self):
  seen=[]
  class Handler(BaseHTTPRequestHandler):
   def log_message(self,*a):pass
   def do_GET(self):
    seen.append(self.path);self.send_response(200);self.end_headers();self.wfile.write(b"PRIVATE_MARKER")
  server=ThreadingHTTPServer(("127.0.0.1",0),Handler)
  threading.Thread(target=server.serve_forever,daemon=True).start()
  self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
  def respond(body):
   results=self.results(body)
   if not results:return tool_response([("read_source",{"url":f"http://127.0.0.1:{server.server_port}/private"})])
   self.assertNotIn("PRIVATE_MARKER",json.dumps(results));self.assertEqual(seen,[])
   return text_response("内网访问已拒绝")
  self.callback=respond
  self.enabled(self.send(self.message("private-source","读取本机页面",user="user-2",scope="group",expected="内网访问已拒绝")))
if __name__=="__main__":unittest.main()
