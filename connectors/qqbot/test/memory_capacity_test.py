"""从旧的 256 条满容量证据恢复，验证新交流继续采集且旧证据不丢失。"""
import copy
import json
import unittest

import memory_test


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


if __name__ == '__main__': unittest.main()
