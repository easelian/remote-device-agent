#!/usr/bin/env python3
"""Serve the browser scenario pages and count what each page writes.

Usage: server.py PORT

GET  /<page>.html      a scenario page from the pages directory
GET  /slow/<ms>        an empty answer after that many milliseconds
GET  /drip/<page>.html the same page, its body held back for 0.7 s after the document begins
POST /w/<scenario>     record one site write; the body is kept as text
GET  /audit            every recorded write, by scenario, as JSON
POST /reset            forget all writes
POST /fail             always answers 500

The counter is the oracle of a scenario: what the site received does not depend
on what a browser tool reported.
"""
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PAGES = Path(__file__).resolve().parent / "pages"
writes, lock = {}, threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, body, kind="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/audit":
            with lock:
                return self.reply(200, json.dumps(writes).encode())
        if path.startswith("/slow/"):
            # An answer that takes as many milliseconds as the address says.
            time.sleep(min(int(path[len("/slow/"):] or 0), 5000) / 1000)
            return self.reply(200, b"{}")
        if path.startswith("/drip/"):
            # The document is begun, and the rest of it comes most of a second later.
            page = PAGES / path[len("/drip/"):]
            if page.suffix == ".html" and page.parent == PAGES and page.is_file():
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(b"<!doctype html>" + b" " * 2048)
                self.wfile.flush()
                time.sleep(0.7)
                self.wfile.write(page.read_bytes())
                self.close_connection = True
                return
            return self.reply(404, b"{}")
        page = PAGES / path.lstrip("/")
        if page.suffix == ".html" and page.parent == PAGES and page.is_file():
            return self.reply(200, page.read_bytes(), "text/html; charset=utf-8")
        self.reply(404, b"{}")

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0)).decode()
        if self.path == "/reset":
            with lock:
                writes.clear()
        elif self.path == "/fail":
            return self.reply(500, b'{"error":"limit exceeded"}')
        elif self.path.startswith("/w/"):
            with lock:
                writes.setdefault(self.path[3:], []).append(body)
        else:
            return self.reply(404, b"{}")
        self.reply(200, b"{}")


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    print(f"ready {server.server_port}", flush=True)
    server.serve_forever()
