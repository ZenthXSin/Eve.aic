"""用相同合成快照、输入与模型响应比较运行器；不连接真实 QQ 或模型站点。"""
from request_messages import conversation_messages
import argparse
import hashlib
import json
import pathlib
import statistics
from http.server import BaseHTTPRequestHandler

import memory_test
import recall_test


def comparable(requests):
    """仅忽略两次独立实验新产生的情景记忆时间，不修改实际请求。"""
    result = json.loads(json.dumps(requests))
    for request in result:
        for message in request['messages']:
            text = message.get('content')
            if message['role'] != 'system' or not isinstance(text,str) or '"kind":"eve.conversation-digest-v1"' not in text:
                continue
            prefix, data = text.split('\n',1)
            data = json.loads(data)
            assert data['kind'] == 'eve.conversation-digest-v1'
            for episode in data['episodes']: episode['at_ms'] = 0
            message['content'] = prefix + '\n' + json.dumps(data,ensure_ascii=False,separators=(',',':'))
    return result


def run(binary, payload_mib, samples):
    memory_test.BINARY = pathlib.Path(binary).resolve(strict=True)
    fixture = recall_test.RecallAcceptance(methodName='runTest')
    fixture.setUp()
    try:
        requests = []

        class Model(BaseHTTPRequestHandler):
            def log_message(self, *_): pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                requests.append(request)
                latest = next(m['content'] for m in reversed(request['messages']) if m['role'] == 'user')
                body = json.dumps({'choices':[{'index':0,'finish_reason':'stop',
                    'message':{'role':'assistant','content':latest}}]}).encode()
                self.send_response(200)
                self.send_header('Content-Type','application/json')
                self.send_header('Content-Length',str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        fixture.server.RequestHandlerClass = Model
        fixture.state_path.parent.mkdir()
        payload = [120] * (payload_mib * 1024 * 1024)
        fixture.state_path.write_text(json.dumps({'version':1,'entries':{
            'eve.synthetic-load':{'payload':payload}}},separators=(',',':')))
        initial_bytes = fixture.state_path.stat().st_size
        script = []
        for number in range(samples):
            script.extend(fixture.send(fixture.message(f'latency-{number}',f'你好，这是第 {number+1} 条合成消息。')))
        fixture.run_eve(script, memory=False, panel=True, env_extra={
            'EVE_TRACE_DIALOGUE_TIMING':'1','EVE_OPENAI_REASONING_EFFORT':'medium',
            'EVE_OPENAI_MAX_OUTPUT_TOKENS':'8192'})
        state = json.loads(fixture.state_path.read_text())
        assert state['entries']['eve.synthetic-load']['payload'] == payload
        assert len(requests) == samples
        for index, request in enumerate(requests):
            assert request['reasoning_effort'] == 'medium'
            assert request['max_tokens'] == 8192
            assert sum(m['role'] == 'user' for m in conversation_messages(request['messages'])) == index + 1
            names = [t['function']['name'] for t in request['tools']]
            assert 'echo' in names
            assert request['tools'] == requests[0]['tools']
        turns = [t for s in json.loads(bytes(state['entries']['eve.session']['sessions.v1']))['sessions'].values() for t in s['turns']]
        assert len(turns) == samples and all(t['status']['state'] == 'Completed' for t in turns)
        phases = {key:[] for key in ['message_received','dispatch','decoded','reply_written','delivery_confirmed']}
        for line in fixture.last_stderr.splitlines():
            if line.startswith(('EVE_QQ_TIMING ','EVE_HTTP_TIMING ')):
                event = json.loads(line.split(' ',1)[1])
                if event['phase'] in phases: phases[event['phase']].append(event['at_ms'])
        assert all(len(values) == samples for values in phases.values())
        metrics = {}
        for name, start, end in [('admission_ms','message_received','dispatch'),
                                 ('synthetic_model_ms','dispatch','decoded'),
                                 ('after_model_ms','decoded','reply_written'),
                                 ('total_ms','message_received','delivery_confirmed')]:
            values = [end-start for start,end in zip(phases[start],phases[end])]
            metrics[name] = {'median':statistics.median(values),'samples':values}
        report = {'binary':str(memory_test.BINARY),'binary_sha256':hashlib.sha256(memory_test.BINARY.read_bytes()).hexdigest(),
                  'initial_state_bytes':initial_bytes,'final_state_bytes':fixture.state_path.stat().st_size,
                  'metrics':metrics,'unchanged_payload':True,'completed_turns':len(turns),
                  'tool_names':[t['function']['name'] for t in requests[0]['tools']]}
        return report, requests
    finally:
        fixture.tearDown()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', action='append', required=True, help='运行器路径；可重复，按顺序比较')
    parser.add_argument('--payload-mib',type=int,choices=range(1,33),default=4)
    parser.add_argument('--samples',type=int,choices=range(2,17),default=5)
    parser.add_argument('--output',type=pathlib.Path)
    args = parser.parse_args()
    reports, baseline = [], None
    for binary in args.binary:
        report, requests = run(binary,args.payload_mib,args.samples)
        if baseline is None: baseline = requests
        report['identical_model_requests'] = requests == baseline
        report['equivalent_model_requests'] = comparable(requests) == comparable(baseline)
        report['comparable_requests_sha256'] = hashlib.sha256(json.dumps(comparable(requests),sort_keys=True).encode()).hexdigest()
        reports.append(report)
    result = {'synthetic_only':True,'samples_per_binary':args.samples,'runs':reports,
              'comparison_ignores_only':['system.eve.conversation-digest-v1.episodes[*].at_ms']}
    if args.output: args.output.write_text(json.dumps(result,ensure_ascii=False,indent=2))
    print(json.dumps(result,ensure_ascii=False))
    assert all(r['equivalent_model_requests'] for r in reports), '模型请求发生非实验时间差异，不能作为不损失质量的比较'


if __name__ == '__main__': main()
