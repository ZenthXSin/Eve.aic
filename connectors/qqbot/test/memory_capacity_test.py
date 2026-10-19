"""从旧的大容量证据恢复：新交流继续采集，较早的证据移入归档分块且一条不丢。"""
import copy
import json
import os
import subprocess
import unittest

import memory_test

MEMORY_CLI = memory_test.BINARY.with_name("eve-memory" + memory_test.BINARY.suffix)


class MemoryCapacityAcceptance(memory_test.MemoryAcceptance):
    for name in dir(memory_test.MemoryAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def test_old_full_snapshot_continues_collecting_without_dropping_sources_or_replaying(self):
        self.remember()
        self.run_eve([*self.send(self.message('seed-interaction','已有交流来源')),
                      *self.checkpoint('seeded')],checkpoints={'seeded':lambda:self.wait_until(
                          lambda:len(self.interactions()) == 1,'seed interaction not imported')})
        state = json.loads(self.state_path.read_text())
        memory = json.loads(bytes(state['entries']['eve.memory']['memory.v1']))
        snapshot = memory['scopes'][0]['snapshot']
        template = self.interactions()[0]
        evidence = copy.deepcopy(snapshot['evidence'])
        for index in range(len(evidence),256):
            item = copy.deepcopy(template)
            item.update(id=f'synthetic-old-{index}',revision=index+1)
            item['source']['message_id'] = f'synthetic-old-message-{index}'
            item['source']['turn_id'] = 1000 + index
            item['source']['session_revision'] = (1000 + index) * 2
            evidence.append(item)
        snapshot.update(revision=256,evidence=evidence)
        state['entries']['eve.memory']['memory.v1'] = list(json.dumps(memory).encode())
        self.state_path.write_text(json.dumps(state,separators=(',',':')))

        def imported():
            self.wait_until(lambda:len(self.only_snapshot()['evidence']) == 257,'new interaction not imported')
        self.run_eve([*self.send(self.message('after-capacity','容量满后仍应正常交流')),
                      *self.checkpoint('imported')],checkpoints={'imported':imported})
        current = self.only_snapshot()
        self.assertEqual(current['evidence'][:256],evidence)
        self.assertEqual(current['preferences'],snapshot['preferences'])
        self.run_eve([*self.send(self.message('after-restart','重启后继续采集')),
                      *self.checkpoint('imported-again')],checkpoints={'imported-again':lambda:self.wait_until(
                          lambda:len(self.only_snapshot()['evidence']) == 258,'second interaction not imported')})
        self.assertEqual(self.only_snapshot()['evidence'][:256],evidence)

    def seed_old_evidence(self, total, at_ms=None):
        """在真实导入的一条交流之后补足 total 条旧证据，模拟升级前已积累的状态。"""
        state = json.loads(self.state_path.read_text())
        memory = json.loads(bytes(state['entries']['eve.memory']['memory.v1']))
        snapshot = memory['scopes'][0]['snapshot']
        template = self.interactions()[0]
        evidence = copy.deepcopy(snapshot['evidence'])
        for index in range(len(evidence), total):
            item = copy.deepcopy(template)
            item.update(id=f'synthetic-old-{index}', revision=index + 1)
            if at_ms is not None:
                item['at_ms'] = at_ms + index
            item['source']['message_id'] = f'synthetic-old-message-{index}'
            item['source']['turn_id'] = 1000 + index
            item['source']['session_revision'] = (1000 + index) * 2
            item['source']['user_text'] = f'旧交流原文 {index}'
            evidence.append(item)
        snapshot.update(revision=total, evidence=evidence)
        state['entries']['eve.memory']['memory.v1'] = list(json.dumps(memory).encode())
        self.state_path.write_text(json.dumps(state, separators=(',', ':')))
        return snapshot

    def memory_cli(self, *args):
        env = {key: os.environ[key] for key in ('PATH', 'SystemRoot', 'TEMP', 'TMP') if key in os.environ}
        result = subprocess.run([str(MEMORY_CLI), '--state-dir', str(self.work / 'state'), *args],
                                env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_large_old_state_archives_into_verified_chunks_and_reads_back_every_source(self):
        self.remember()
        self.run_eve([*self.send(self.message('seed-interaction', '已有交流来源')),
                      *self.checkpoint('seeded')], checkpoints={'seeded': lambda: self.wait_until(
                          lambda: len(self.interactions()) == 1, 'seed interaction not imported')})
        # 1100 条旧证据：超过热层归档阈值 1024；旧时间戳早于 8 天保护期。
        seeded = self.seed_old_evidence(1100, at_ms=1000)
        before = seeded['evidence']

        def imported():
            self.wait_until(lambda: self.only_snapshot()['revision'] == 1101, 'new interaction not imported')
        self.run_eve([*self.send(self.message('after-capacity', '归档后仍应正常交流')),
                      *self.checkpoint('imported')], checkpoints={'imported': imported})
        memory = self.memory()
        self.assertEqual(memory['format_version'], 2)
        current = self.only_snapshot()
        archived_count = current['archived_evidence']
        self.assertEqual(archived_count, 256)
        self.assertEqual(len(current['evidence']) + archived_count, 1101)
        hot_ids = {item['id'] for item in current['evidence']}
        moved = [item for item in before if item['id'] not in hot_ids]
        self.assertEqual(len(moved), archived_count)
        # 偏好命令与首条真实交流仍在热层；被移出的都是旧的完成交互。
        self.assertTrue(all(item['source']['kind'] == 'CompletedInteraction' for item in moved))
        self.assertEqual(len(list((self.work / 'state/memory-archive').iterdir())), 1)

        scope = current['scope']
        scope_args = ['--channel', scope['channel'], '--session', scope['session_id'], '--user', scope['user_id']]
        status = self.memory_cli('archive-status')['archive']
        self.assertEqual((status['chunks'], status['archived_evidence'], status['unreferenced_chunks']),
                         (1, 256, 0))
        self.assertEqual(self.memory_cli(*scope_args, 'status')['archived_evidence_count'], 256)
        rows, after = [], 0
        while True:
            page = self.memory_cli(*scope_args, 'archived', '--after', str(after), '--limit', '64',
                                   '--include-content')
            rows.extend(page['archived'])
            if page['next_after'] is None:
                break
            after = page['next_after']
        self.assertEqual([row['id'] for row in rows], [item['id'] for item in moved])
        self.assertEqual([row['revision'] for row in rows], [item['revision'] for item in moved])
        self.assertTrue(all(item['source']['user_text'] in json.dumps(row, ensure_ascii=False)
                            for row, item in zip(rows, moved)))

        self.run_eve([*self.send(self.message('after-restart', '重启后继续采集')),
                      *self.checkpoint('imported-again')], checkpoints={'imported-again': lambda: self.wait_until(
                          lambda: self.only_snapshot()['revision'] == 1102, 'second interaction not imported')})
        self.assertEqual(self.only_snapshot()['archived_evidence'], 256)


if __name__ == '__main__': unittest.main()
