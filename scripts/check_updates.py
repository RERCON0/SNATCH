"""Check reviewed toolchain/installer pins against official upstream metadata."""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Upstream metadata unexpectedly redirected; review the origin")


def fetch(url, *, token=None, max_bytes=1024 * 1024):
    allowed = ("https://api.github.com/", "https://static.rust-lang.org/dist/")
    if not url.startswith(allowed):
        raise ValueError("Unapproved metadata origin")
    if token and not url.startswith(allowed[0]):
        raise ValueError("GitHub token may only be sent to api.github.com")
    headers = {"User-Agent": "SNATCH-dependency-watch", "Accept": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    began = time.monotonic()
    opener = urllib.request.build_opener(NoRedirect())
    for attempt in range(3):
        remaining = 60 - (time.monotonic() - began)
        if remaining <= 0:
            raise TimeoutError("Metadata request deadline exceeded")
        try:
            with opener.open(urllib.request.Request(url, headers=headers), timeout=min(10, remaining)) as response:
                if response.status != 200:
                    raise ValueError("Unexpected upstream status")
                length = response.headers.get("Content-Length")
                if length is not None and not 0 <= int(length) <= max_bytes:
                    raise ValueError("Metadata Content-Length exceeds limit")
                data = bytearray()
                while True:
                    if time.monotonic() - began >= 60:
                        raise TimeoutError("Metadata request deadline exceeded")
                    chunk = response.read1(min(65536, max_bytes - len(data) + 1))
                    if not chunk:
                        break
                    data.extend(chunk)
                    if len(data) > max_bytes:
                        raise ValueError("Metadata exceeds size limit")
                if length is not None and len(data) != int(length):
                    raise http.client.IncompleteRead(b"", int(length) - len(data))
                return bytes(data)
        except urllib.error.HTTPError as error:
            if error.code not in (408, 429, 500, 502, 503, 504) or attempt == 2:
                raise
        except urllib.error.URLError as error:
            if not isinstance(error.reason, (TimeoutError, ConnectionError)) or attempt == 2:
                raise
        except (TimeoutError, ConnectionError, http.client.IncompleteRead):
            if attempt == 2:
                raise
        time.sleep(attempt + 1)
    raise AssertionError("Retry limit exceeded")


def compare(rust_pin, upstream_rust, aria_pin, upstream_aria):
    updates = []
    if rust_pin != upstream_rust:
        updates.append(f"Rust: reviewed {rust_pin}, upstream stable {upstream_rust}")
    if aria_pin not in upstream_aria:
        updates.append(f"aria2: pinned asset {aria_pin} absent from latest release; review checksum and compatibility")
    return updates


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fail-on-updates", action="store_true")
    args = parser.parse_args()
    rust_pin = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    setup = (ROOT / "src/setup.rs").read_text(encoding="utf-8")
    aria_pin = re.search(r'const PINNED_ARIA2_ZIP: &str = "([^"]+)";', setup).group(1)
    rust = tomllib.loads(fetch("https://static.rust-lang.org/dist/channel-rust-stable.toml", max_bytes=2 * 1024 * 1024).decode())
    upstream_rust = rust["pkg"]["rust"]["version"].split()[0]
    aria = json.loads(fetch("https://api.github.com/repos/aria2/aria2/releases/latest", token=os.environ.get("GITHUB_TOKEN")))
    updates = compare(rust_pin, upstream_rust, aria_pin, [asset["name"] for asset in aria["assets"]])
    print("\n".join(updates) if updates else "Reviewed Rust and aria2 pins match upstream.")
    if args.fail_on_updates and updates:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
