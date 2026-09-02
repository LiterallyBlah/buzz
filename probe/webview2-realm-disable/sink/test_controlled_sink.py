import importlib.util
import pathlib
import struct
import unittest

MODULE = pathlib.Path(__file__).with_name('controlled_sink.py')
spec = importlib.util.spec_from_file_location('controlled_sink', MODULE)
assert spec and spec.loader
sink = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sink)


class SinkProtocolTests(unittest.TestCase):
    def message(self, kind, attrs=b''):
        return struct.pack('!HH', kind, len(attrs)) + sink.MAGIC + bytes(range(12)) + attrs

    def test_stun_binding_response_is_protocol_valid(self):
        request = self.message(0x0001)
        response = sink.stun_binding_success(request, ('127.0.0.1', 3478))
        self.assertIsNotNone(response)
        parsed = sink.parse_message(response)
        self.assertEqual(parsed['type'], 0x0101)
        self.assertEqual(parsed['transaction'], bytes(range(12)))

    def test_turn_allocate_extracts_nonce_bound_username(self):
        username = b'buzz:0123456789abcdef01234567:control-initial:offhost'
        request = self.message(0x0003, sink.attribute(0x0006, username))
        response, observed = sink.turn_challenge(request, 'realm', 'nonce')
        self.assertEqual(observed, username.decode())
        parsed = sink.parse_message(response)
        self.assertEqual(parsed['type'], 0x0113)
        self.assertIn(0x0014, parsed['attrs'])
        self.assertIn(0x0015, parsed['attrs'])

    def test_malformed_messages_fail_closed(self):
        self.assertIsNone(sink.parse_message(b'not-stun'))
        self.assertIsNone(sink.turn_challenge(b'not-turn', 'realm', 'nonce'))


if __name__ == '__main__':
    unittest.main()
