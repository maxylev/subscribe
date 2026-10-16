"""Mock Telegram Bot API for the e2e test.

Usage: mock_tg.py <port> <workdir>

* Updates to deliver are injected by dropping JSON files into
  <workdir>/inbox/ (each file holds one update object); they are served by
  getUpdates in file-name order and then deleted.
* Every sendMessage / answerCallbackQuery body is appended as one JSON line
  to <workdir>/tg_sent.jsonl.
"""

import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
WORKDIR = sys.argv[2]
INBOX = os.path.join(WORKDIR, "inbox")
SENT = os.path.join(WORKDIR, "tg_sent.jsonl")
os.makedirs(INBOX, exist_ok=True)


def take_updates():
    updates = []
    for name in sorted(os.listdir(INBOX)):
        if not name.endswith(".json"):
            continue
        path = os.path.join(INBOX, name)
        with open(path, encoding="utf-8") as f:
            updates.append(json.load(f))
        os.remove(path)
    return updates


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        payload = json.loads(self.rfile.read(length) or b"{}")
        method = self.path.rsplit("/", 1)[-1]
        if method == "getMe":
            self.reply({"ok": True, "result": {"id": 1, "is_bot": True, "first_name": "mock", "username": "mockbot"}})
        elif method == "getUpdates":
            # Emulate long polling briefly so the server doesn't spin.
            deadline = time.time() + min(float(payload.get("timeout", 0)), 1.0)
            updates = take_updates()
            while not updates and time.time() < deadline:
                time.sleep(0.1)
                updates = take_updates()
            self.reply({"ok": True, "result": updates})
        elif method in ("sendMessage", "answerCallbackQuery"):
            payload["method"] = method
            with open(SENT, "a", encoding="utf-8") as f:
                f.write(json.dumps(payload, ensure_ascii=False) + "\n")
            self.reply({"ok": True, "result": True if method == "answerCallbackQuery" else {"message_id": 1}})
        else:
            self.reply({"ok": False, "error_code": 404, "description": "Not Found"}, 404)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
