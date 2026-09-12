#!/usr/bin/env python3
"""Reference Potemkin plugin, in Python, demonstrating the cross-language protocol.

It speaks newline-delimited JSON over stdio: one request object per line in, one
response object per line out. This trivial plugin rewrites the type text
`DemoType<...>` into a friendlier `Demo{...}` form to show the mechanism.

Run by Potemkin via a manifest (see demo.json); not meant to be run by hand.
"""
import json
import re
import sys

NAME = "demo"
MARKERS = ["DemoType<"]

_PATTERN = re.compile(r"DemoType<([^>]*)>")


def transform_one(text: str) -> str:
    return _PATTERN.sub(lambda m: "Demo{" + m.group(1) + "}", text)


def handle(req: dict) -> dict:
    method = req.get("method")
    if method == "initialize":
        return {"id": req["id"], "result": {"name": NAME, "markers": MARKERS}}
    if method == "transform":
        params = req.get("params") or {}
        items = params.get("items") or []
        return {
            "id": req["id"],
            "result": {"items": [transform_one(it.get("text", "")) for it in items]},
        }
    if method == "shutdown":
        return {"id": req["id"], "result": {}}
    return {"id": req.get("id", 0), "error": f"unknown method: {method}"}


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
            resp = handle(req)
        except Exception as e:  # never crash the proxy's session
            resp = {"id": 0, "error": str(e)}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
        if req.get("method") == "shutdown":
            break


if __name__ == "__main__":
    main()
