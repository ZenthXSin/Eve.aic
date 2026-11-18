from datetime import datetime, timezone
from http.client import HTTPConnection
import json
from pathlib import Path
import re
import sys
from urllib.parse import urlsplit


def read(path):
    return json.loads(Path(path).read_text())


def rpc(config, method, args=None, channel='private'):
    connection = read(config['adminConnectionFile' if channel == 'admin' else 'connectionFile'])
    phone = channel == 'phone'
    address = urlsplit(connection['phone']['adapterUrl'] if phone else connection['url'])
    if address.scheme != 'http' or address.hostname != '127.0.0.1' or not address.port:
        raise ValueError('invalid local endpoint')
    key = 'adapterKeyFile' if phone else 'adminKeyFile' if channel == 'admin' else 'keyFile'
    payload = {'method': method, 'request' if address.path == '/admin-control' else 'args': args or {}}
    client = HTTPConnection('127.0.0.1', address.port, timeout=15)
    try:
        client.request('POST', address.path, json.dumps(payload), {
            'Authorization': 'Bearer ' + Path(config[key]).read_text().strip(),
            'Content-Type': 'application/json',
        })
        result = json.loads(client.getresponse().read(2_000_001))
        if result.get('ok') is not True:
            raise RuntimeError('RPC failed')
        return result['value']
    finally:
        client.close()


def inspect_owner_history(fetch, now, report):
    cursor = None
    unknown_age = False
    for page_index in range(4):
        page = fetch(cursor)
        rows = sorted(page['messages'], key=lambda row: row['handle']['seq'], reverse=True)
        for index, row in enumerate(rows):
            direction = row['direction']
            stamp = row['time'].get('at')
            delivery = (row.get('delivery') or {}).get('state')
            if page_index == 0 and index == 0:
                report.update(owner_last_direction=direction, owner_last_at=stamp)
                if direction == 'outgoing':
                    report['owner_last_delivery'] = delivery
            if direction == 'outgoing' and delivery == 'sent':
                report['owner_last_confirmed_send_seq'] = row['handle']['seq']
                break
            if direction == 'incoming':
                if stamp is None:
                    unknown_age = True
                elif (now - datetime.fromisoformat(stamp.replace('Z', '+00:00'))).total_seconds() >= 1800:
                    report['owner_unanswered_seq'] = row['handle']['seq']
                    report['issues'].append('owner_dm_no_later_outgoing_30m')
                    return
        else:
            older = page.get('older')
            if older:
                if cursor and older['beforeSeq'] >= cursor['beforeSeq']:
                    raise ValueError('invalid history cursor')
                cursor = older
                continue
        break
    else:
        report['issues'].append('owner_dm_history_inconclusive')
    if unknown_age:
        report['issues'].append('owner_dm_age_unknown')


def inspect_instance(live, report):
    if not isinstance(live, dict):
        raise ValueError('invalid instance status')
    state = live.get('state') if isinstance(live.get('state'), str) else None
    eve = live.get('eve') if isinstance(live.get('eve'), dict) else {}
    running, healthy = eve.get('running'), eve.get('healthy')
    report.update(instance=state,
                  running=running if isinstance(running, bool) else None,
                  healthy=healthy if isinstance(healthy, bool) else None)
    if (state and state != 'active') or running is False or healthy is False:
        probe = 'unhealthy'
    elif state == 'active' and running is True and healthy is True:
        probe = 'unhealthy' if live.get('error') else 'healthy'
    else:
        probe = 'unknown'
    report['instance_probe'] = probe
    if probe == 'unhealthy':
        report['issues'].append('instance_unhealthy')
    elif probe == 'unknown':
        report['issues'].append('instance_probe_unknown')


def main():
    now = datetime.now(timezone.utc)
    report = {'at': now.isoformat(), 'issues': []}
    issues = report['issues']

    def check(label, action):
        try:
            return action()
        except Exception as error:
            issues.append(label + ':' + type(error).__name__)
            return {}

    config = check('supervisor_config', lambda: read('/etc/eve-rebuild/supervisor.json')['supervisor'])
    status = check('control', lambda: rpc(config, 'status', channel='admin'))
    state, backend, ownership = (status.get(section, {}) for section in ('supervisor', 'runtime', 'ownership'))
    current = state.get('current')
    report.update(paused=state.get('paused'), grant_pending=bool(state.get('pendingGrant')),
                  ownership=ownership.get('mode'), permits=ownership.get('activePermits'),
                  runtime=backend.get('available'), timeline_seq=backend.get('throughSeq'))
    if state.get('paused') or report['grant_pending'] or ownership.get('mode') != 'active':
        issues.append('eve_not_active')
    if not backend.get('available') or backend.get('model', {}).get('blocked'):
        issues.append('runtime_or_model_blocked')
    if isinstance(current, str) and re.fullmatch(r'[A-Za-z0-9_-]+', current):
        instance = check('instance_config', lambda: read(f'/etc/eve-rebuild/instances/{current}/config.json'))
        phone = check('qq_health', lambda: rpc(instance['phone'], 'health', channel='phone'))
        report['gateway'] = phone.get('gateway', {}).get('status')
        if (phone.get('available') is not True or
                report['gateway'] not in ('ready', 'resumed', 'webhook_verified')):
            issues.append('qq_unavailable')
        report.update(instance=None, running=None, healthy=None, instance_probe='unknown')
        check('instance', lambda: inspect_instance(
            rpc(instance['instance'], 'status', {'brief': True}), report))
    else:
        issues.append('no_current_instance')

    def page(resource, args):
        return rpc(config, 'query', {'resource': resource, 'operation': 'page', 'args': args}, 'admin')

    def inspect_history():
        owner = read('/etc/eve-rebuild/qq.json')['qq']['modelControl']['ownerPlatformUserId']
        def fetch(cursor):
            args = {'conversationId': 'qq:user:' + owner}
            if cursor:
                args['cursor'] = cursor
            return page('qqHistory', args)
        inspect_owner_history(fetch, now, report)

    def inspect_queue():
        heads = page('qqFailures', {'limit': 32})
        report['qq_failed_heads_sample'] = len(heads['items'])
        if heads['items']:
            issues.append('qq_send_queue_needs_review')

    def inspect_events():
        head = backend['throughSeq']
        after = max(0, head - 32)
        events = []
        while after < head:
            batch = page('timeline', {'afterSeq': after, 'throughSeq': head})['items']
            if not batch or batch[-1]['seq'] <= after:
                raise ValueError('invalid cursor')
            events.extend(batch)
            after = batch[-1]['seq']
        recent = [event for event in events if (now - datetime.fromisoformat(event['at'].replace('Z', '+00:00'))).total_seconds() <= 3600]
        report.update(last_event_at=events[-1]['at'] if events else None,
                      model_failures_sample=sum(event['kind'] == 'model_failure' for event in recent),
                      tool_failures_sample=sum(event['kind'] == 'tool_result' and event.get('data', {}).get('result', {}).get('status') == 'failed' for event in recent))

    check('private_history', inspect_history)
    check('send_queue', inspect_queue)
    if backend.get('available'):
        check('recent_timeline', inspect_events)
    print(json.dumps(report, separators=(',', ':')), flush=True)
    return int(bool(issues))


if __name__ == '__main__':
    sys.exit(main())
