#!/bin/sh
# Create the tinysweeper GitHub App via GitHub's app-manifest flow.
#
# There is no API to create a GitHub App outright — the manifest flow is the
# supported path, and it needs exactly one click in a browser. This script does
# everything either side of that click: it serves the auto-submitting form,
# catches the redirect, converts the temporary code into real credentials, and
# writes them out.
#
# Usage:
#   scripts/create-github-app.sh <org> [port] [public-host]
#
# The browser that completes the flow does not have to be on this machine. Pass
# a <public-host> reachable from wherever you are browsing — a Tailscale name,
# for instance — and the listener binds 0.0.0.0 so it can be reached. Bind
# address and redirect host are the same decision: GitHub redirects the
# *browser*, so `localhost` only works when the browser is here too.
#
# On success it writes app credentials to .tinysweeper-app.json in the current
# directory. That file contains a private key: it is gitignored, and you should
# move it into a secret store and delete it.

set -eu

ORG="${1:-}"
PORT="${2:-8901}"
PUBLIC_HOST="${3:-localhost}"
MANIFEST="$(dirname "$0")/../deploy/github-app-manifest.json"
OUT=".tinysweeper-app.json"

usage() {
    cat <<EOF
Usage: $0 <org> [port] [public-host]

  <org>           GitHub organisation to create the app under, e.g. tinyhumansai
  [port]          Port for the redirect listener (default 8901)
  [public-host]   Host the browser will reach this listener on (default
                  localhost). Set it to a Tailscale name or LAN address to
                  complete the flow from another machine; the listener then
                  binds 0.0.0.0.

Requires: python3, and a browser that can reach <public-host>:<port>.
EOF
}

if [ -z "$ORG" ]; then
    usage
    exit 2
fi

if [ ! -f "$MANIFEST" ]; then
    echo "error: manifest not found at $MANIFEST" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "error: python3 is required" >&2
    exit 1
fi

echo "Creating GitHub App 'tinysweeper' under org '$ORG'."
echo "A browser will open. Review the permissions and click 'Create GitHub App'."
echo

ORG="$ORG" PORT="$PORT" PUBLIC_HOST="$PUBLIC_HOST" MANIFEST="$MANIFEST" OUT="$OUT" python3 - <<'PYTHON'
import http.server
import json
import os
import socketserver
import threading
import urllib.request
import webbrowser

org = os.environ["ORG"]
port = int(os.environ["PORT"])
public_host = os.environ.get("PUBLIC_HOST", "localhost")
out = os.environ["OUT"]

# Bind everywhere when the browser is elsewhere. Keeping the loopback binding
# for the local case avoids exposing a listener that nothing needs to reach.
bind = "127.0.0.1" if public_host in ("localhost", "127.0.0.1") else "0.0.0.0"

with open(os.environ["MANIFEST"]) as handle:
    manifest = json.load(handle)

# The redirect has to come back to this listener, and the webhook URL is only a
# placeholder until the server is actually deployed.
manifest["redirect_url"] = f"http://{public_host}:{port}/callback"

code_box = {}
done = threading.Event()

FORM = """<!doctype html>
<html><body>
<p>Redirecting to GitHub…</p>
<form id="f" action="https://github.com/organizations/{org}/settings/apps/new" method="post">
  <input type="hidden" name="manifest" id="m">
</form>
<script>
  document.getElementById("m").value = {manifest};
  document.getElementById("f").submit();
</script>
</body></html>"""

DONE_PAGE = """<!doctype html>
<html><body style="font-family:system-ui;padding:3rem">
<h2>tinysweeper app created</h2>
<p>Credentials written to <code>{out}</code>. You can close this tab.</p>
</body></html>"""


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/callback"):
            query = self.path.split("?", 1)[1] if "?" in self.path else ""
            params = dict(
                part.split("=", 1) for part in query.split("&") if "=" in part
            )
            code_box["code"] = params.get("code")
            body = DONE_PAGE.format(out=out).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            done.set()
            return

        body = FORM.format(org=org, manifest=json.dumps(json.dumps(manifest))).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer((bind, port), Handler) as httpd:
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()

    url = f"http://{public_host}:{port}/"
    print(f"Open this in a browser: {url}")
    if bind == "127.0.0.1":
        webbrowser.open(url)

    if not done.wait(timeout=600):
        raise SystemExit("timed out waiting for GitHub to redirect back")
    httpd.shutdown()

code = code_box.get("code")
if not code:
    raise SystemExit("GitHub redirected without a code; app was not created")

# One-time exchange: this endpoint works exactly once per code, and it is the
# only time GitHub ever hands over the private key.
request = urllib.request.Request(
    f"https://api.github.com/app-manifests/{code}/conversions",
    method="POST",
    headers={"Accept": "application/vnd.github+json", "User-Agent": "tinysweeper"},
)
with urllib.request.urlopen(request) as response:
    app = json.load(response)

with open(out, "w") as handle:
    json.dump(app, handle, indent=2)
os.chmod(out, 0o600)

print()
print(f"  app:      {app['name']} (id {app['id']})")
print(f"  slug:     {app['slug']}")
print(f"  written:  {out}  (contains the private key — move it to a secret store)")
print()
print(f"  Install it: {app['html_url']}/installations/new")
PYTHON
