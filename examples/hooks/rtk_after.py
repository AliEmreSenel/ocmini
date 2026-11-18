#!/usr/bin/env python3
"""Optional ocmini after-tool hook; see docs/hooks-examples.md."""
import json
import os
import re
import stat
import subprocess
import sys
from pathlib import Path

# RTK v1.2's stdin filter rejects input above this size.
RTK_INPUT_BYTES = 10 * 1024 * 1024


def read_spill(path):
    path = Path(path)
    if not path.is_absolute() or ".." in path.parts or os.open not in os.supports_dir_fd:
        return None
    flags = os.O_RDONLY | os.O_NOFOLLOW
    directory = os.open(path.anchor, flags | os.O_DIRECTORY)
    try:
        for name in path.parts[1:-1]:
            child = os.open(name, flags | os.O_DIRECTORY, dir_fd=directory)
            os.close(directory)
            directory = child
        fd = os.open(path.name, flags | os.O_NONBLOCK, dir_fd=directory)
        with os.fdopen(fd, "rb") as spill:
            info = os.fstat(spill.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > RTK_INPUT_BYTES:
                return None
            raw = spill.read(RTK_INPUT_BYTES + 1)
        return raw.decode("utf-8") if len(raw) <= RTK_INPUT_BYTES else None
    finally:
        os.close(directory)


def transform(request):
    if not isinstance(request, dict) or request.get("hook") != "tool.execute.after":
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
    raw = read_spill(path)
    if raw is None:
        return {}
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
    output = result.stdout.rstrip() + "\n\n[Original raw tool output: " + path + "]"
    return {"set": {"output": output}} if len(output.encode()) < len(data["output"].encode()) else {}


def main():
    try:
        response = transform(json.load(sys.stdin))
    except (OSError, ValueError, TypeError, subprocess.SubprocessError):
        response = {}
    print(json.dumps(response))


if __name__ == "__main__":
    main()
