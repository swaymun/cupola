#!/usr/bin/env python3
"""A complete Cupola RPC client: no pairing, stdio to `cupola rpc` as the owner.

Lists each Project's most recent threads, then follows host events from a
checkpoint for a few seconds. Usage:

    tests/contracts/rpc/example_client.py [--seconds N] [--cupola PATH]
"""
import argparse
import json
import queue
import subprocess
import threading
import time


class Wonder:
    def __init__(self, command):
        self.process = subprocess.Popen(
            [command, "rpc"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
        )
        self.next_id = 0
        self.replies = {}
        self.events = queue.Queue()
        self.ready = threading.Condition()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            message = json.loads(line)
            if "method" in message:  # a notification
                self.events.put(message)
                continue
            with self.ready:
                self.replies[message["id"]] = message
                self.ready.notify_all()

    def call(self, method, params=None):
        self.next_id += 1
        request = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            request["params"] = params
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        with self.ready:
            self.ready.wait_for(lambda: self.next_id in self.replies, timeout=30)
            reply = self.replies.pop(self.next_id, None)
        if reply is None:
            raise RuntimeError(f"{method}: no answer")
        if "error" in reply:
            raise RuntimeError(f"{method}: {reply['error']['message']} ({reply['error']['code']})")
        return reply["result"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=float, default=5)
    parser.add_argument("--cupola", default="cupola")
    args = parser.parse_args()
    wonder = Wonder(args.cupola)

    hello = wonder.call("initialize", {
        "protocolVersion": 1,
        "clientInfo": {"name": "example-client", "version": "1"},
    })
    print(f"connected as {hello['authority']}; {len(hello['methods'])} methods")

    # Take the cursor before reading, so no event falls between the two.
    checkpoint = wonder.call("events.checkpoint")
    for project in wonder.call("projects.list")["projects"]:
        page = wonder.call("threads.list", {"projectId": project["id"], "limit": 5})
        print(f"{project['name']}: {len(page['threads'])} recent threads")
        for thread in page["threads"]:
            print(f"  - {thread['title']} ({thread['family']})")

    cursor = {"hostEpoch": checkpoint["hostEpoch"], "sequence": checkpoint["lastSequence"]}
    subscription = wonder.call("events.subscribe", {"after": cursor})
    print(f"following events after #{cursor['sequence']} for {args.seconds:g}s")
    deadline = time.monotonic() + args.seconds
    while (left := deadline - time.monotonic()) > 0:
        try:
            note = wonder.events.get(timeout=left)
        except queue.Empty:
            break
        if note["method"] == "events.ended":
            print(f"subscription ended ({note['params']['reason']}); read state again")
            break
        event = note["params"]["event"]
        cursor["sequence"] = event["sequence"]  # save once applied
        print(f"  event #{event['sequence']}: {event['event']['type']}")
    wonder.call("events.unsubscribe", {"subscriptionId": subscription["subscriptionId"]})
    wonder.process.stdin.close()
    wonder.process.wait(timeout=10)
    print(f"done; resume from {json.dumps(cursor)}")


if __name__ == "__main__":
    main()
