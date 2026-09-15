#!/usr/bin/env python3
"""Answer captive-portal probes on port 80, and redirect strays to the dashboard.

Installed as /usr/local/sbin/pump-portal-responder by setup-hotspot.sh and
run by pump-portal.service.

This is a SEPARATE PROCESS from pump_control.py on purpose. Nothing in the
protection path may ever share a process with something that binds a socket
for the convenience of a phone. If this crashes, wedges or fails to bind,
the pump keeps running and the dashboard on :8080 is unaffected.

What it buys: Android stops nagging "this network has no internet" and
stops demoting the WiFi route in favour of cellular, which is the failure
that makes a perfectly good Pi look dead. What it does NOT buy: modern
Android runs an HTTPS probe alongside the HTTP one, and that cannot be
satisfied without a certificate the phone already trusts. So this reduces
the problem; putting the phone in airplane mode before joining is what
actually removes it.
"""

import http.server
import socketserver
import sys

AP_ADDR = "__AP_ADDR__"
WEB_PORT = "__WEB_PORT__"

DASHBOARD = f"http://{AP_ADDR}:{WEB_PORT}/"

# Apple's probe compares the body byte-for-byte, so this string is exact.
APPLE_SUCCESS = (
    b"<HTML><HEAD><TITLE>Success</TITLE></HEAD><BODY>Success</BODY></HTML>\n"
)

# Android's probe endpoints. Anything else gets redirected, so a stray tap
# on the "sign in to network" notification lands on the dashboard instead
# of an error page.
NO_CONTENT_PATHS = ("/generate_204", "/gen_204")
APPLE_PATHS = ("/hotspot-detect.html", "/library/test/success.html")


class Portal(http.server.BaseHTTPRequestHandler):
    server_version = "pump-portal"
    protocol_version = "HTTP/1.1"

    def _respond(self, code, body=b"", ctype="text/html", extra=None):
        self.send_response(code)
        if code == 204:
            # RFC 7230: a 204 MUST NOT carry Content-Length. The client
            # already knows there is no body from the status code, so
            # keep-alive framing still works. Omitting it avoids tripping
            # a strict probe client on an unrecoverable code path.
            pass
        else:
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
        # No caching: a phone that caches a 204 will not re-probe after the
        # AP is fixed, and you would never see the state change.
        self.send_header("Cache-Control", "no-store")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path in NO_CONTENT_PATHS:
            self._respond(204)
        elif path in APPLE_PATHS:
            self._respond(200, APPLE_SUCCESS)
        else:
            # Redirect to the name the phone actually asked for, so someone
            # who typed reisland.lan does not land on a bare IP. Falls back
            # to the AP address for probes that send no Host header.
            host = self.headers.get("Host", AP_ADDR).split(":")[0] or AP_ADDR
            self._respond(302, extra={"Location": f"http://{host}:{WEB_PORT}/"})

    def do_HEAD(self):
        self.do_GET()

    def log_message(self, fmt, *args):
        # Default BaseHTTPRequestHandler logging writes a line per probe,
        # and a joined phone probes every few seconds. On a box with no log
        # rotation for this unit that is pure noise in the journal.
        pass


class Server(socketserver.ThreadingTCPServer):
    # A phone that half-opens a connection must not hold the port across a
    # restart of this unit.
    allow_reuse_address = True
    daemon_threads = True


def main():
    try:
        with Server(("0.0.0.0", 80), Portal) as httpd:
            httpd.serve_forever()
    except OSError as e:
        # Losing port 80 is not a reason to fail loudly and have systemd
        # restart-loop. The dashboard is on 8080 and is unaffected.
        print(f"portal not started: {e}", file=sys.stderr)
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
