#!/usr/bin/python3
"""仅管理 Eve 成员 veth 的公网规则，不重写既有防火墙。"""
import os
import subprocess

RULES = r"""
table inet eve_members {
 set private4 { type ipv4_addr; flags interval; elements = { 0.0.0.0/8,10.0.0.0/8,100.64.0.0/10,127.0.0.0/8,169.254.0.0/16,172.16.0.0/12,192.0.0.0/24,192.0.2.0/24,192.88.99.0/24,192.168.0.0/16,198.18.0.0/15,198.51.100.0/24,203.0.113.0/24,224.0.0.0/3 } }
 chain input { type filter hook input priority -10; policy accept; iifname "eve-m-*" counter drop; }
 chain forward { type filter hook forward priority -10; policy accept;
  iifname "eve-m-*" ip daddr @private4 counter drop
  iifname "eve-m-*" meta nfproto ipv6 counter drop
  iifname "eve-m-*" meta l4proto != { tcp, udp } counter drop
  oifname "eve-m-*" ct state != { established, related } counter drop
 }
 chain postrouting { type nat hook postrouting priority srcnat; policy accept; ip saddr 10.254.0.0/16 oifname != "eve-m-*" masquerade; }
}
"""

def run(*args,**kwargs): return subprocess.run(args,check=True,stdout=subprocess.DEVNULL,**kwargs)
if __name__ == "__main__":
 if os.geteuid()!=0:raise SystemExit("requires root")
 if subprocess.run(["/usr/sbin/nft","list","table","inet","eve_members"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode:
  run("/usr/sbin/nft","-f","-",input=RULES,text=True)
 # nft 先拒绝内网，Docker 的默认 FORWARD DROP 仍需只对 Eve veth 放行。
 for rule in [["-i","eve-m+","-j","ACCEPT"],["-o","eve-m+","-m","conntrack","--ctstate","ESTABLISHED,RELATED","-j","ACCEPT"]]:
  if subprocess.run(["/usr/sbin/iptables","-C","DOCKER-USER",*rule],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode:
   run("/usr/sbin/iptables","-A","DOCKER-USER",*rule)
 run("/usr/sbin/sysctl","-q","-w","net.ipv4.ip_forward=1")
