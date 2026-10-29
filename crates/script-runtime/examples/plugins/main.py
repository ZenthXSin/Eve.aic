"""Python 父脚本：通过宿主调用已绑定的 JS 子版本，无额外权限。"""
import json
import sys

def receive():
    return json.loads(sys.stdin.readline())

def rpc(request):
    print(json.dumps(request, ensure_ascii=False), flush=True)
    response = receive()
    if not response["ok"]:
        raise RuntimeError("host rejected")
    return response["result"]

invocation = receive()
report = rpc({"op": "script", "dependency": "report", "arguments": invocation["arguments"]})
print(json.dumps({"op": "return", "value": report}, ensure_ascii=False), flush=True)
