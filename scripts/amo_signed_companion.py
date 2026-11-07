"""Download the AMO-signed Firefox companion for the version its manifest names.

Exit status: 0 when the signed file is written, 3 when the version is still
awaiting review once the wait ends, 1 on any other failure. Credentials come
from AMO_JWT_ISSUER and AMO_JWT_SECRET.
"""

import argparse
import base64
import hashlib
import hmac
import json
import os
from pathlib import Path
import secrets
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

API_URL = "https://addons.mozilla.org/api/v5/"
PENDING = 3
SIGNATURE = "META-INF/mozilla.rsa"


class Pending(Exception):
    pass


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def jwt(issuer, secret):
    now = int(time.time())
    head = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    claims = {"iss": issuer, "jti": secrets.token_hex(16), "iat": now, "exp": now + 60}
    body = b64url(json.dumps(claims).encode())
    signature = hmac.new(secret.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{b64url(signature)}"


def get(url, issuer, secret):
    request = urllib.request.Request(url, headers={
        "Authorization": f"JWT {jwt(issuer, secret)}",
        "Accept": "application/json",
        "User-Agent": "bobby-browser-release",
    })
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


def companion_identity(manifest_path):
    manifest = json.loads(Path(manifest_path).read_text())
    return manifest["browser_specific_settings"]["gecko"]["id"], manifest["version"]


def approved_file_url(api_url, guid, version, issuer, secret, wait, interval):
    """Poll the version until its file is public; `Pending` when the wait ends first."""
    url = urllib.parse.urljoin(
        api_url,
        f"addons/addon/{urllib.parse.quote(guid, safe='@')}/versions/v{urllib.parse.quote(version)}/",
    )
    deadline = time.monotonic() + wait
    while True:
        status = json.loads(get(url, issuer, secret))["file"]
        if status["status"] == "public":
            return status["url"]
        if status["status"] != "unreviewed":
            raise RuntimeError(f"{guid} {version} file status is {status['status']}")
        if time.monotonic() + interval > deadline:
            raise Pending(f"{guid} {version} is awaiting review on addons.mozilla.org")
        time.sleep(interval)


def write_signed(payload, out):
    """Write `payload` to `out` only if it carries a Mozilla signature."""
    out = Path(out)
    with tempfile.NamedTemporaryFile(dir=out.parent or ".", suffix=".xpi", delete=False) as temp:
        temp.write(payload)
    try:
        with zipfile.ZipFile(temp.name) as archive:
            if SIGNATURE not in archive.namelist():
                raise RuntimeError(f"downloaded file carries no {SIGNATURE}")
        os.replace(temp.name, out)
    finally:
        if os.path.exists(temp.name):
            os.unlink(temp.name)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--wait", type=float, default=0, help="seconds to wait for approval")
    parser.add_argument("--interval", type=float, default=30, help="seconds between checks")
    parser.add_argument("--api-url", default=API_URL)
    args = parser.parse_args(argv)
    issuer = os.environ.get("AMO_JWT_ISSUER", "")
    secret = os.environ.get("AMO_JWT_SECRET", "")
    try:
        if not issuer or not secret:
            raise RuntimeError("AMO_JWT_ISSUER and AMO_JWT_SECRET are required")
        guid, version = companion_identity(args.manifest)
        file_url = approved_file_url(
            args.api_url, guid, version, issuer, secret, args.wait, args.interval
        )
        write_signed(get(file_url, issuer, secret), args.out)
    except Pending as pending:
        print(pending, file=sys.stderr)
        return PENDING
    except (RuntimeError, KeyError, ValueError, OSError, urllib.error.URLError, zipfile.BadZipFile) as error:
        print(f"amo_signed_companion: {error}", file=sys.stderr)
        return 1
    print(f"wrote {args.out} for {guid} {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
