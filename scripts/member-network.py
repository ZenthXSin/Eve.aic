#!/usr/bin/python3
"""Linux root 宿主的成员命令公网出口；每次命令独立网络，无宿主或内网访问。"""
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import uuid

IP = "/usr/sbin/ip"
NFT = "/usr/sbin/nft"
ROOT = Path("/run/eve-members")

def run(*args, **kwargs):
    return subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, **kwargs)

def identity(pid):
    try: return Path(f"/proc/{pid}/stat").read_text().rsplit(")",1)[1].split()[19]
    except (OSError,IndexError): return None

def cleanup(name):
    subprocess.run([IP,"netns","delete",name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    subprocess.run([IP,"link","delete",name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    (ROOT/(name+".json")).unlink(missing_ok=True)

def allocate():
    ROOT.mkdir(mode=0o700,exist_ok=True)
    with (ROOT/"lock").open("a") as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        used=set()
        for file in ROOT.glob("eve-m-*.json"):
            row=json.loads(file.read_text())
            if identity(row["pid"])!=row["start"]: cleanup(file.stem)
            else: used.add(row["slot"])
        slot=next((n for n in range(1,16383) if n not in used),None)
        if slot is None: raise RuntimeError("成员网络容量已满")
        name="eve-m-"+uuid.uuid4().hex[:8]
        (ROOT/(name+".json")).write_text(json.dumps({"slot":slot,"pid":os.getpid(),"start":identity(os.getpid())}))
        return name,slot

def main():
    if os.geteuid() != 0:
        raise SystemExit("成员公网出口需要可信 root 宿主；未执行命令")
    if len(sys.argv)<2 or sys.argv[1]!="/usr/bin/bwrap":
        raise SystemExit("网络启动器只接受宿主构造的 bubblewrap 命令")
    name, slot = allocate()
    child = None
    stopping = False
    def stop(_sig,_frame):
        nonlocal stopping
        stopping = True
        if child is not None and child.poll() is None: child.terminate()
    signal.signal(signal.SIGTERM,stop)
    signal.signal(signal.SIGINT,stop)
    try:
        # 所有命令各有点对点 /30，既不共享宿主也不共享其他成员的环回。
        run(NFT,"list","table","inet","eve_members")
        run(IP,"netns","add",name)
        run(IP,"link","add",name,"type","veth","peer","name","eth0","netns",name)
        third, fourth = divmod(slot * 4, 256)
        host = f"10.254.{third}.{fourth+1}"
        peer = f"10.254.{third}.{fourth+2}"
        run(IP,"addr","add",host+"/30","dev",name)
        run(IP,"link","set",name,"up")
        run(IP,"-n",name,"addr","add",peer+"/30","dev","eth0")
        run(IP,"-n",name,"link","set","eth0","up")
        run(IP,"-n",name,"link","set","lo","up")
        run(IP,"-n",name,"route","add","default","via",host)
        if stopping: return 143
        # 子进程只能看到自己的网络命名空间；移除 CAP_NET_ADMIN 后不能改路由或防火墙。
        child = subprocess.Popen([IP,"netns","exec",name,*sys.argv[1:]],env=os.environ.copy())
        return child.wait()
    except subprocess.CalledProcessError:
        print("成员公网出口初始化失败；未回退宿主网络",file=sys.stderr)
        return 1
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            try: child.wait(timeout=3)
            except subprocess.TimeoutExpired: child.kill();child.wait()
        cleanup(name)

if __name__ == "__main__":
    raise SystemExit(main())
