#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Send one trivial Jev request through Cloudflare AI Gateway and report the result.

Default mode reuses the JevClient from scripts/jev-shadow.py, so a pass proves the
nightly shadow job's endpoint, headers, and body shape. `--probe` additionally runs
free read-only checks (token verify, gateway settings) and, on HTTP 403, retries with
alternative header sets so the failing piece can be identified. Credentials are read
from the environment and are never printed.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any
from urllib.parse import quote

API_ROOT = "https://api.cloudflare.com/client/v4"
STATE = "The build finished and every test passed."
QUESTIONS = {
    "build_ok": {
        "type": "noul",
        "instructions": "Does the state report a successful build?",
        "criteria": {"true": "The build succeeded", "false": "The build failed or is unknown"},
    }
}


def load_shadow() -> Any:
    path = Path(__file__).with_name("jev-shadow.py")
    spec = importlib.util.spec_from_file_location("jev_shadow", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def redact(text: str, secrets: tuple[str, ...]) -> str:
    for secret in secrets:
        if secret:
            text = text.replace(secret, "[redacted]").replace(quote(secret, safe=""), "[redacted]")
    return re.sub(r"(?i)\bBearer\s+\S+", "Bearer [redacted]", text)


def http(
    method: str, url: str, headers: dict[str, str], body: bytes | None = None
) -> tuple[int, dict[str, str], bytes]:
    request = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=90) as response:
            return response.status, dict(response.headers), response.read(65_536)
    except urllib.error.HTTPError as exc:
        try:
            raw = exc.read(65_536)
        finally:
            exc.close()
        return exc.code, dict(exc.headers), raw


def show(label: str, status: int, headers: dict[str, str], raw: bytes, secrets: tuple[str, ...]) -> None:
    ray = next((v for k, v in headers.items() if k.lower() == "cf-ray"), "-")
    print(f"[{label}] HTTP {status} (cf-ray {ray})")
    text = raw.decode("utf-8", errors="replace")
    try:
        text = json.dumps(json.loads(text), indent=2, sort_keys=True)
    except json.JSONDecodeError:
        pass
    print(redact(text, secrets)[:4000])


def summarize_success(raw: bytes) -> bool:
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError):
        return False
    result = parsed.get("result", parsed) if isinstance(parsed, dict) else None
    if not isinstance(result, dict) or not isinstance(result.get("answers"), dict):
        return False
    print(f"result.model: {result.get('model')}")
    print(f"result.usage: {json.dumps(result.get('usage'), sort_keys=True)}")
    print(f"result.answers: {json.dumps(result.get('answers'), sort_keys=True)}")
    return True


def probe(account_id: str, api_token: str, gateway_id: str, gateway_token: str) -> int:
    secrets = (api_token, gateway_token, account_id, gateway_id)
    account = quote(account_id, safe="")
    auth = {"Authorization": f"Bearer {api_token}"}

    for label, url in (
        ("token verify (user)", f"{API_ROOT}/user/tokens/verify"),
        ("token verify (account)", f"{API_ROOT}/accounts/{account}/tokens/verify"),
    ):
        status, headers, raw = http("GET", url, auth)
        show(label, status, headers, raw, secrets)
        if status == 200:
            break

    if gateway_id:
        status, headers, raw = http(
            "GET", f"{API_ROOT}/accounts/{account}/ai-gateway/gateways/{quote(gateway_id, safe='')}", auth
        )
        show("gateway settings", status, headers, raw, secrets)

    body = json.dumps({"model": "typesafe/jev", "input": {"state": STATE, "questions": QUESTIONS}}).encode()
    base = {**auth, "Content-Type": "application/json"}
    variants: list[tuple[str, dict[str, str]]] = []
    if gateway_id:
        variants.append(
            (
                "Authorization + cf-aig-authorization + cf-aig-gateway-id",
                {**base, "cf-aig-authorization": f"Bearer {gateway_token}", "cf-aig-gateway-id": gateway_id},
            )
        )
        variants.append(("Authorization + cf-aig-gateway-id", {**base, "cf-aig-gateway-id": gateway_id}))
    variants.append(("Authorization only (no gateway)", base))

    url = f"{API_ROOT}/accounts/{account}/ai/run"
    for label, headers in variants:
        status, response_headers, raw = http("POST", url, headers, body)
        show(f"jev: {label}", status, response_headers, raw, secrets)
        if 200 <= status < 300 and summarize_success(raw):
            print(f"PASS with header set: {label}")
            return 0
        if status != 403:
            break
    print("FAIL: no header set returned a Jev answer")
    return 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", action="store_true", help="run read-only checks and header variants")
    args = parser.parse_args()

    account_id = os.environ.get("CLOUDFLARE_ACCOUNT_ID", "").strip()
    api_token = os.environ.get("CLOUDFLARE_API_TOKEN", "").strip()
    gateway_id = os.environ.get("CLOUDFLARE_AI_GATEWAY_ID", "").strip()
    gateway_token = os.environ.get("CLOUDFLARE_AI_GATEWAY_TOKEN", "").strip() or api_token
    if not account_id or not api_token:
        print("CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN are required", file=sys.stderr)
        return 2
    print(f"gateway id present: {bool(gateway_id)}; separate gateway token: {gateway_token != api_token}")

    if args.probe:
        return probe(account_id, api_token, gateway_id, gateway_token)

    shadow = load_shadow()
    client = shadow.JevClient(account_id, api_token, gateway_id or None, gateway_token)
    try:
        result = client.call(STATE, QUESTIONS)
    except RuntimeError as exc:
        print(f"FAIL: {exc}")
        return 1
    print(f"result.model: {result.get('model')}")
    print(f"result.usage: {json.dumps(result.get('usage'), sort_keys=True)}")
    print(f"result.answers: {json.dumps(result.get('answers'), sort_keys=True)}")
    print("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
