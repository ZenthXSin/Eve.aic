"""真实网络命名空间验收；无 QQ、无生产凭据。必须先部署 member-network-setup。"""
import concurrent.futures
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import sys
import threading

ROOT=Path(__file__).resolve().parent
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_GET(self):self.send_response(200);self.end_headers();self.wfile.write(b"HOST_ONLY")
server=ThreadingHTTPServer(("0.0.0.0",0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
port=server.server_port
addresses=json.loads(subprocess.check_output(["ip","-j","address"]))
host=next(a["local"] for i in addresses for a in i["addr_info"] if a["family"]=="inet" and a["scope"]=="global" and i["ifname"]=="ens18")
probe="""import json,socket,urllib.request
blocked=[]
for host,port in TARGETS:
 try:
  s=socket.create_connection((host,port),timeout=.8);s.close();blocked.append(False)
 except OSError:blocked.append(True)
print(json.dumps({'blocked':blocked,'public_https':urllib.request.urlopen('https://example.com',timeout=12).status}))
""".replace("TARGETS",repr([("127.0.0.1",port),(host,port),("10.254.0.1",port),("169.254.169.254",80),("172.17.0.1",port)]))
base=[sys.executable,str(ROOT/'member-network.py'),'/usr/bin/bwrap','--unshare-all','--share-net','--die-with-parent','--cap-drop','ALL','--ro-bind','/usr','/usr','--ro-bind','/lib','/lib','--ro-bind','/lib64','/lib64','--ro-bind','/etc/resolv.conf','/etc/resolv.conf','--ro-bind','/etc/ssl/certs','/etc/ssl/certs','--proc','/proc','--dev','/dev','--','/usr/bin/python3','-c',probe]
def run(_):
 p=subprocess.run(base,text=True,capture_output=True,timeout=25)
 assert p.returncode==0,p.stderr
 result=json.loads(p.stdout);assert all(result['blocked']) and result['public_https']==200,result
 return result
try:
 with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:results=list(pool.map(run,range(2)))
 assert not [x for x in Path('/run/netns').glob('eve-m-*')], 'network namespace leaked'
 print(json.dumps({'parallel_runs':results,'cleanup':True}))
finally:server.shutdown();server.server_close()
