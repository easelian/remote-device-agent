#!/usr/bin/env python3
"""A forwarding proxy for the browser scenarios.

Usage: proxy.py PORT LOG

A request for a host under .test is passed to the same port on this machine,
which is how a test reaches the scenario site by a name that resolves nowhere.
Every host asked for is appended to LOG, one to a line, before it is answered:
what the proxy was asked is the oracle, not what a browser tool reported.
"""
import http.client
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

LOG = sys.argv[2]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def note(self, host):
        with open(LOG, "a", encoding="utf-8") as log:
            log.write(host + "\n")

    def refuse(self):
        self.send_response(502)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def forward(self):
        url = urlsplit(self.path)
        host = url.hostname or ""
        self.note(host)
        if not host.endswith(".test") or not url.port:
            return self.refuse()
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {name: value for name, value in self.headers.items()
                   if not name.lower().startswith("proxy-") and name.lower() != "host"}
        site = http.client.HTTPConnection("127.0.0.1", url.port, timeout=10)
        site.request(self.command, url.path + ("?" + url.query if url.query else ""), body, headers)
        answer = site.getresponse()
        data = answer.read()
        self.send_response(answer.status)
        self.send_header("Content-Type", answer.getheader("Content-Type") or "text/plain")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_POST = forward

    def do_CONNECT(self):
        self.note(self.path.rsplit(":", 1)[0])
        self.refuse()


server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
print("ready", flush=True)
server.serve_forever()
