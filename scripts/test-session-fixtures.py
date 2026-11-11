"""Contracts: no source data escapes; generated histories match runtime events."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from copy import deepcopy

spec = importlib.util.spec_from_file_location('fixtures', Path(__file__).with_name('generate-session-fixtures.py'))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)


class SessionFixtureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        (fixtures.ROOT / '.local').mkdir(mode=0o700, exist_ok=True)
        cls.profile = json.loads(fixtures.PROFILE.read_text())
        cls.bundles = {p: fixtures.generate(cls.profile, p, count=4) for p in ('codex', 'claude')}

    def test_committed_profile_can_only_contain_reviewed_enums_and_numbers(self):
        fixtures.validate_profile(self.profile)
        for mutation in ('sourcePath', 'copiedToolName'):
            bad = deepcopy(self.profile)
            if mutation == 'sourcePath': bad['codex']['sourcePath'] = '/Users/fixture/session.jsonl'
            else: bad['codex']['templates'][0][1][0] = 'unreviewed_source_tool'
            with self.assertRaises(ValueError): fixtures.validate_profile(bad)

    def test_sampler_discards_source_values_and_preserves_late_call_pairing(self):
        # Entirely fictional input. A late result still belongs to its first turn.
        sentinel = 'fictional-person@example.invalid sk-FAKESECRET0123456789 /Users/fixture/private'
        def record(second, payload):
            return {'type': 'response_item', 'timestamp': f'2026-01-01T00:00:{second:02}Z', 'payload': payload}
        rows = [record(0, {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': sentinel}]}),
                record(1, {'type': 'function_call', 'call_id': sentinel, 'name': sentinel, 'arguments': sentinel}),
                record(2, {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': 'Another synthetic turn'}]}),
                record(3, {'type': 'function_call_output', 'call_id': sentinel, 'output': sentinel}),
                record(4, {'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': 'A fictional reply'}]})]
        with tempfile.TemporaryDirectory(dir=fixtures.ROOT / '.local') as directory:
            path = Path(directory) / 'session.jsonl'
            path.write_text('\n'.join(json.dumps(r) for r in rows))
            sampled = fixtures.sample(Path(directory), 'codex', limit=3)
        fixtures.privacy_scan(sampled)
        serialized = json.dumps(sampled)
        self.assertNotIn('Fictional', serialized)
        self.assertNotIn('FAKESECRET', serialized)
        call = next(row for template in sampled['templates'] for row in template if row[0] == 'dynamicToolCall')
        self.assertEqual(call[1:6], [len(sentinel), len(sentinel), 1000, 2000, False])

    def test_sampler_bounds_reads_and_does_not_use_truncated_last_turn(self):
        prefix = json.dumps({'type': 'response_item', 'timestamp': '2026-01-01T00:00:00Z', 'payload': {'type': 'message', 'role': 'user', 'content': [{'type': 'text', 'text': 'Synthetic'}]}})
        with tempfile.TemporaryDirectory(dir=fixtures.ROOT / '.local') as directory:
            (Path(directory) / 'large.jsonl').write_text(prefix + '\n' + 'x' * 2048)
            with self.assertRaisesRegex(ValueError, 'No complete sampled turns'):
                fixtures.sample(Path(directory), 'codex', limit=3, byte_limit=1024)

    def test_privacy_scan_rejects_fictional_sensitive_values_and_filenames(self):
        values = ['sk-FAKESECRET0123456789', 'Bearer FICTIONALONLY', 'api_key=fictional',
                  '-----BEGIN PRIVATE KEY-----', 'fictional.person@example.invalid', '+1 (202) 555-0198',
                  '2025550198', '/Users/fixture/notes', '/home/fixture/notes', 'C:\\Users\\Fictional',
                  'fictional-host.local', 'https://example.invalid', 'fictional.tailabcd.ts.net',
                  '192.0.2.17', '2001:db8:0:1::1', 'Ab9_' * 16]
        for value in values:
            with self.subTest(value=value):
                with self.assertRaises(ValueError): fixtures.privacy_scan({'nested': [value]})
        for value in ({'apiKey': 'fictional'}, {'hostname': 'fictional'}):
            with self.assertRaises(ValueError): fixtures.privacy_scan(value)
        for name in ('fictional@example.invalid.json', 'fictional-host.local.json', 'fictional-host.local', '202-555-0198.json'):
            with self.assertRaises(ValueError): fixtures.privacy_filename(name)

    def test_privacy_scan_accepts_only_verified_generated_binary_fields(self):
        for bundle in self.bundles.values(): fixtures.privacy_scan(bundle)
        fixtures.privacy_scan({'text': 'Open todos.js and notes.txt', 'durationMs': 123456789})
        for key in ('base64', 'sha256'):
            with self.assertRaises(ValueError): fixtures.privacy_scan({key: 'FICTIONAL_UNREVIEWED_ENCODING'})

    def test_pinned_schema_and_stream_lifecycle_reject_corruption(self):
        cases = []
        missing = deepcopy(self.bundles['codex']); del missing['threadRead']['thread']['projectId']; cases.append(missing)
        delta = deepcopy(self.bundles['codex'])
        next(r for r in delta['replay'] if r['message'].get('method') == 'item/agentMessage/delta')['message']['params']['delta'] += 'altered'
        cases.append(delta)
        approval = deepcopy(self.bundles['codex'])
        next(r for r in approval['replay'] if r['direction'] == 'client')['message']['id'] = 'unknown-request'
        cases.append(approval)
        order = deepcopy(self.bundles['codex']); order['replay'][1]['atMs'] = -1; cases.append(order)
        asset = deepcopy(self.bundles['codex']); asset['assets'][0]['byteSize'] += 1; cases.append(asset)
        outcome = deepcopy(self.bundles['codex']); outcome['threadRead']['thread']['turns'][0]['status'] = 'failed'; cases.append(outcome)
        for bad in cases:
            with self.assertRaises(ValueError): fixtures.validate_bundle(bad)

    def test_reproducible_bundles_use_provider_owned_shapes(self):
        for provider, bundle in self.bundles.items():
            self.assertEqual(bundle, fixtures.generate(self.profile, provider, count=4))
            self.assertEqual(len(bundle['threadRead']['thread']['turns']), 4)
            items = [i for t in bundle['threadRead']['thread']['turns'] for i in t['items']]
            types = {i['type'] for i in items}
            self.assertTrue({'userMessage', 'agentMessage', 'plan', 'commandExecution', 'fileChange', 'mcpToolCall'} <= types)
            self.assertIn('collabAgentToolCall' if provider == 'codex' else 'subAgentActivity', types)
            if provider == 'claude':
                self.assertEqual(bundle['threadRead']['thread']['source'], 'sdk')
                self.assertTrue(any(i['type'] == 'commandExecution' and 'aggregatedOutput' in i for i in items))
                self.assertNotIn('webSearch', types)
            self.assertTrue(any(len(i.get('text', '')) >= 32768 for i in items))

    def test_call_duration_is_not_added_twice_to_the_next_item_time(self):
        turns = [{'id': 'timing-turn', 'status': 'completed', 'items': [
            fixtures.item_for(['commandExecution', 10, 10, 0, 1000, False, 'final_answer'], 'command', 0, 'codex'),
            {'type': 'agentMessage', 'id': 'reply', 'text': 'Synthetic reply', 'phase': 'final_answer'}]}]
        replay = fixtures.codex_replay(turns, {'command': {'gapMs': 0, 'durationMs': 1000}, 'reply': {'gapMs': 1200, 'durationMs': 0}}, 0)
        reply = next(r for r in replay if r['message'].get('method') == 'item/completed' and r['message']['params']['item']['id'] == 'reply')
        self.assertEqual(reply['atMs'], 1200)
        self.assertEqual(turns[0]['durationMs'], 1200)

    def test_started_tools_do_not_reveal_their_future_results(self):
        for row in self.bundles['codex']['replay']:
            if row['message'].get('method') != 'item/started': continue
            item = row['message']['params']['item']
            if item['type'] == 'commandExecution':
                self.assertIsNone(item['aggregatedOutput'])
                self.assertIsNone(item['exitCode'])
            elif item['type'] == 'mcpToolCall': self.assertIsNone(item['result'])
            elif item['type'] == 'dynamicToolCall': self.assertIsNone(item['contentItems'])

    def test_measured_lengths_survive_synthesis_with_documented_minimums(self):
        for size in (128, 1024, fixtures.MAX_TEXT):
            for kind in ('userMessage', 'agentMessage', 'commandExecution'):
                item = fixtures.item_for([kind, size, size, 500, 1000, False, 'final_answer'], 'synthetic-item', 0, 'codex')
                actual = item['content'][0]['text'] if kind == 'userMessage' else item['text'] if kind == 'agentMessage' else item['command']
                self.assertEqual(len(actual), size)
                if kind == 'commandExecution': self.assertEqual(len(item['aggregatedOutput']), size)


if __name__ == '__main__': unittest.main()
