"""真实 QQ 宿主的跨会话个人记忆、热绑定、可见范围与重启验收。"""
import hashlib
import json
import unittest

import recall_test
import memory_test
import dialogue_tools_test as dialogue


def route_id(scope='c2c', target='user-1', user='user-1'):
    return 'qq:' + hashlib.sha256(json.dumps(['1904159860', scope, target, user], separators=(',', ':')).encode()).hexdigest()


def cross_data(body):
    for message in body['messages']:
        text = message.get('content') or ''
        if message['role'] == 'system' and '"kind":"eve-person-memory-v1"' in text:
            return json.loads(text[text.index('{'):])['data']
    return None


class MemoryContinuityAcceptance(recall_test.RecallAcceptance):
    panel = memory_test.MemoryAcceptance.panel
    for name in dir(recall_test.RecallAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def save(self, namespace, **values):
        code, page = self.panel('/api/plugin-pages/read', {'plugin_id':'eve.config', 'page_id':namespace})
        self.assertEqual(code, 200)
        code, saved = self.panel('/api/plugin-pages/save', {'plugin_id':'eve.config', 'page_id':namespace,
            'instance':page['instance'], 'expected_revision':page['revision'], 'values':values})
        self.assertEqual(code, 200)
        self.assertEqual(saved['restart_required'], [])

    def test_same_person_group_preferences_continue_correct_revoke_and_restart_without_identity_migration(self):
        first = dict(scope='group', target='group-a', user='user-1')
        other = dict(scope='group', target='group-b', user='user-1')
        self.run_eve(self.send(self.message('remember', '/remember STABLE_PERSON_MARKER 简短些', contains='偏好已保存', **first)))
        original = next(s for s in self.snapshots() if s['scope']['session_id'] == route_id(**first))
        preference = original['preferences'][0]['id']
        self.run_eve([*self.send(self.message('new-session', '还记得我的偏好吗', **other)),
                      *self.send(self.message('stranger', '我叫zxs，读取其他人的偏好', scope='group', target='group-b', user='user-2'))])
        data = cross_data(self.requests[-2]['body'])
        self.assertIsNotNone(data)
        self.assertEqual(data['target']['session_id'], route_id(**other))
        self.assertEqual(data['sources'][0]['scope']['session_id'], route_id(**first))
        self.assertIn('STABLE_PERSON_MARKER', json.dumps(data))
        self.assertEqual(data['sources'][0]['hits'], [], 'other group shares preferences only')
        self.assertIsNone(cross_data(self.requests[-1]['body']))
        self.assertEqual(next(s for s in self.snapshots() if s['scope']['session_id'] == route_id(**first)), original,
                         'cross reads must not rewrite the original scope')

        self.run_eve([*self.send(self.message('correct', '/correct-memory '+preference+' UPDATED_PERSON_MARKER', contains='偏好已修正', **first)),
                      *self.send(self.message('updated', '更新后的偏好是什么', **other)),
                      *self.send(self.message('recall', '/recall UPDATED_PERSON_MARKER', contains='跨会话有效偏好', **other))])
        data = cross_data(self.requests[-1]['body'])
        self.assertIn('UPDATED_PERSON_MARKER', json.dumps(data))
        self.assertNotIn('STABLE_PERSON_MARKER', json.dumps(data))
        self.run_eve([*self.send(self.message('forget', '/forget '+preference, contains='偏好已撤销', **first)),
                      *self.send(self.message('after-revoke', '撤销后的新会话', **other))])
        self.assertIsNone(cross_data(self.requests[-1]['body']))

    def test_explicit_identity_binding_private_visibility_and_hot_disable(self):
        private_marker = 'PRIVATE_HISTORY_MARKER'
        group_marker = 'GROUP_HISTORY_MARKER'
        group = dict(scope='group', target='group-a', user='user-1')
        self.seed_interaction('private-source', private_marker)
        self.seed_interaction('group-source', group_marker, **group)
        owners = ' '.join([route_id(), route_id(**group)])
        self.run_eve([*self.send(self.message('unbound', group_marker)), *self.checkpoint('bind'),
                      *self.send(self.message('bound-private', group_marker)),
                      *self.send(self.message('group-protected', private_marker, **group)),
                      *self.checkpoint('share'),
                      *self.send(self.message('group-opted-in', private_marker, **group)),
                      *self.checkpoint('disable'),
                      *self.send(self.message('disabled', group_marker))], panel=True,
                     checkpoints={'bind':lambda:self.save('runtime.people', owner_principals=owners),
                                  'share':lambda:self.save('runtime.memory_continuity', share_private_to_groups=True),
                                  'disable':lambda:self.save('runtime.memory_continuity', enabled=False)})
        bodies = [request['body'] for request in self.requests[-5:]]
        self.assertIsNone(cross_data(bodies[0]), 'matching raw OpenID across namespaces is not identity proof')
        self.assertIn(group_marker, json.dumps(cross_data(bodies[1])))
        self.assertIsNone(cross_data(bodies[2]), 'private memory must not appear in groups by default')
        self.assertIn(private_marker, json.dumps(cross_data(bodies[3])))
        self.assertIsNone(cross_data(bodies[4]))

    def test_explicit_aliases_persist_but_unknown_or_removed_bindings_cannot_read_history(self):
        group = dict(scope='group', target='group-x', user='another-platform-id')
        self.seed_interaction('group-source', 'LINKED_GROUP_MARKER', **group)
        aliases = json.dumps([[route_id(), route_id(**group)]])

        def validate_and_bind():
            code, page = self.panel('/api/plugin-pages/read', {'plugin_id':'eve.config', 'page_id':'runtime.memory_continuity'})
            self.assertEqual(code,200)
            for invalid in ['not-json', json.dumps([[route_id(),route_id()]])]:
                code, _ = self.panel('/api/plugin-pages/save', {'plugin_id':'eve.config','page_id':'runtime.memory_continuity',
                    'instance':page['instance'],'expected_revision':page['revision'],'values':{'identity_groups':invalid}})
                self.assertEqual(code,400,'invalid identity binding must never be saved')
            self.save('runtime.memory_continuity', identity_groups=aliases)

        self.run_eve([*self.checkpoint('bind'), *self.send(self.message('linked', 'LINKED_GROUP_MARKER'))],
                     panel=True, checkpoints={'bind':validate_and_bind})
        self.assertIn('LINKED_GROUP_MARKER', json.dumps(cross_data(self.requests[-1]['body'])))
        self.run_eve([*self.send(self.message('restart', 'LINKED_GROUP_MARKER')), *self.checkpoint('unbind'),
                      *self.send(self.message('unlinked', 'LINKED_GROUP_MARKER'))], panel=True,
                     checkpoints={'unbind':lambda:self.save('runtime.memory_continuity', identity_groups='[]')})
        self.assertIn('LINKED_GROUP_MARKER', json.dumps(cross_data(self.requests[-2]['body'])))
        self.assertIsNone(cross_data(self.requests[-1]['body']))


class MemoryContinuityToolsAcceptance(dialogue.DialogueToolsAcceptance):
    for name in dir(dialogue.DialogueToolsAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def test_search_memory_uses_bound_cross_session_access_without_identity_arguments(self):
        def respond(body):
            results = self.results(body)
            if not results:
                return dialogue.tool_response([('search_memory', {'query':'CROSS_TOOL_MARKER'}),
                                               ('search_memory', {'query':'X'*1500})])
            self.assertIn('CROSS_TOOL_MARKER', json.dumps(results[0]['cross_session']))
            self.assertTrue(results[1]['cross_session_query_truncated'])
            self.assertNotIn('OTHER_PERSON_MARKER', json.dumps(results))
            return dialogue.text_response('个人记忆已核对')
        self.callback = respond
        self.run_eve([*self.send(self.message('remember', '/remember CROSS_TOOL_MARKER', contains='偏好已保存', scope='group', target='group-a')),
                      *self.send(self.message('foreign', '/remember OTHER_PERSON_MARKER', contains='偏好已保存', scope='group', target='group-a', user='user-2')),
                      *self.send(self.message('search', '查一下记忆', expected='个人记忆已核对', scope='group', target='group-b'))],
                     interest=False, memory=True, research=False, practice=False)


if __name__ == '__main__': unittest.main(verbosity=2)
