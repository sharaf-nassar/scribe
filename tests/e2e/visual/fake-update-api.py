#!/usr/bin/env python3
"""Stand-in for the GitHub releases API the Scribe server polls for updates.

`scribe-server` reads its release feed from `SCRIBE_UPDATE_API_URL` (see
`crates/scribe-server/src/updater.rs::api_url`), so pointing that at this
process makes the server announce a real `UpdateAvailable` broadcast and run a
real download when a client sends `TriggerUpdate` — no protocol stubbing and no
patched client.

Routes:
  GET /releases/latest  the GitHub `releases/latest` payload, naming a version
                        far above any shipped one plus the platform asset and
                        its detached `.minisig`
  GET /download/<name>  the named asset. The `.deb` is streamed slowly so the
                        client's "Downloading..." status-bar label is on screen
                        long enough to screenshot; the `.minisig` is deliberate
                        junk, so verification fails and the server broadcasts
                        `UpdateProgress::Failed` instead of installing anything
                        into the container.

Environment:
  FAKE_UPDATE_PORT       listen port (default 8099)
  FAKE_UPDATE_VERSION    tag to announce (default 99.0.0)
  FAKE_UPDATE_SECONDS    seconds to spend streaming the .deb (default 8)
"""

import http.server
import json
import os
import sys
import time

PORT = int(os.environ.get("FAKE_UPDATE_PORT", "8099"))
VERSION = os.environ.get("FAKE_UPDATE_VERSION", "99.0.0")
DOWNLOAD_SECONDS = float(os.environ.get("FAKE_UPDATE_SECONDS", "8"))

BASE = f"http://127.0.0.1:{PORT}"
# The server picks the asset whose name ends with the running platform's
# suffix, so both Linux architectures are listed and either one matches.
ASSET_NAMES = [
    f"scribe_{VERSION}_linux-x86_64.deb",
    f"scribe_{VERSION}_linux-arm64.deb",
]
CHUNK = b"\0" * 65536
CHUNK_COUNT = 16


def release_payload():
    assets = []
    for name in ASSET_NAMES:
        assets.append({"name": name, "browser_download_url": f"{BASE}/download/{name}"})
        assets.append(
            {
                "name": f"{name}.minisig",
                "browser_download_url": f"{BASE}/download/{name}.minisig",
            }
        )
    return {
        "tag_name": f"v{VERSION}",
        "html_url": f"https://example.test/scribe/releases/v{VERSION}",
        "assets": assets,
        "draft": False,
        "prerelease": False,
    }


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("fake-update-api: " + (fmt % args) + "\n")

    def do_GET(self):  # noqa: N802 - http.server's required spelling
        if self.path.startswith("/releases/latest"):
            self.send_json(release_payload())
        elif self.path.startswith("/download/"):
            self.send_asset(self.path.rsplit("/", 1)[-1])
        else:
            self.send_error(404, "no such route")

    def send_json(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_asset(self, name):
        if name.endswith(".minisig"):
            # Not a valid minisign signature: the server rejects it and reports
            # UpdateProgress::Failed, which is the terminal state under test.
            body = b"untrusted comment: fake\nnot-a-real-signature\n"
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if name not in ASSET_NAMES:
            self.send_error(404, "no such asset")
            return
        total = len(CHUNK) * CHUNK_COUNT
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(total))
        self.end_headers()
        delay = DOWNLOAD_SECONDS / CHUNK_COUNT
        for _ in range(CHUNK_COUNT):
            self.wfile.write(CHUNK)
            self.wfile.flush()
            time.sleep(delay)


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    sys.stderr.write(f"fake-update-api: serving v{VERSION} on {BASE}\n")
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
