#!/usr/bin/env python3
"""Optional ocmini after-tool hook; see docs/hooks-examples.md."""
import json
import re
import subprocess
import sys
from pathlib import Path


def transform(request):
    if request.get("hook") != "tool.execute.after":
        return {}
    context = request.get("context")
    data = request.get("input")
    if not isinstance(context, dict) or context.get("tool") not in ("bash", "shell") or not isinstance(data, dict):
        return {}
    args, meta = data.get("args"), data.get("metadata")
    if not isinstance(args, dict) or not isinstance(meta, dict) or data.get("status") != "completed":
        return {}
    if type(meta.get("exit")) is not int or meta["exit"] != 0 or meta.get("truncated") is not True:
        return {}
    if not isinstance(data.get("output"), str) or not isinstance(meta.get("outputPath"), str) or not meta["outputPath"]:
        return {}
    # Exact commands avoid guessing at shell syntax, wrappers, flags, or pipelines.
    name = {"cargo test": "cargo-test", "pytest": "pytest"}.get(args.get("command"))
    if name is None:
        return {}
    path = meta["outputPath"]
    raw = Path(path).read_text(encoding="utf-8")
    # Refuse unexpected output and failures even if a wrapper reported exit zero.
    if name == "cargo-test":
        valid = re.search(r"(?m)^test result: ok\. \d+ passed; 0 failed;", raw)
        failed = re.search(r"(?m)^test result: FAILED\.|^error: test failed", raw)
    else:
        valid = re.search(r"(?m)^=+\s+\d+ passed(?:[, ].*?)?\s+=+\s*$", raw)
        failed = re.search(r"(?m)^=+.*(?:failed|error).*={2,}\s*$", raw, re.IGNORECASE)
    if not valid or failed:
        return {}
    result = subprocess.run(["rtk", "pipe", "--filter", name], input=raw, capture_output=True, text=True, check=True)
    if not result.stdout.strip():
        return {}
    # This wording intentionally differs from ocmini's generated spill suffix:
    # relimiting a changed output removes that suffix and regenerates metadata.
    return {"set": {"output": result.stdout.rstrip() + "\n\n[Original raw tool output: " + path + "]"}}


def main():
    try:
        response = transform(json.load(sys.stdin))
    except (OSError, ValueError, TypeError, subprocess.SubprocessError):
        response = {}
    print(json.dumps(response))


if __name__ == "__main__":
    main()
