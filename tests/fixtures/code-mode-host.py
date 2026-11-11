#!/usr/bin/env python3
"""Synthetic framed host for daemon tests; never executes source or tools."""
import json
import struct
import sys

def send(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(struct.pack('<I', len(body)) + body)
    sys.stdout.buffer.flush()

while True:
    header = sys.stdin.buffer.read(4)
    if len(header) != 4:
        break
    size = struct.unpack('<I', header)[0]
    if size > 1048576:
        sys.exit(1)
    request = json.loads(sys.stdin.buffer.read(size))
    if request['type'] == 'connection/hello':
        send({'type': 'connection/ready'})
    elif request['request']['method'] == 'session/open':
        send({'type': 'operation/response', 'id': request['id'], 'result': {'status': 'ok', 'value': {'type': 'session/ready'}}})
    elif request['request']['method'] == 'session/execute':
        assert request['request']['request']['source'] == 'text(6 * 7)'
        assert request['request']['request']['enabled_tools'] == []
        send({'type': 'execute/initialResponse', 'id': request['id'], 'result': {'status': 'ok', 'value': {'Result': {'content_items': [{'type': 'input_text', 'text': '42'}], 'error_text': None}}}})
    else:
        sys.exit(1)
