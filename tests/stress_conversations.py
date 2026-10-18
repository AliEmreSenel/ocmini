#!/usr/bin/env python3
"""Local deterministic stress test; no credentials or external services required.

python3 tests/stress_conversations.py /path/to/ocmini --mode turns --sizes 100 500 2000
python3 tests/stress_conversations.py /path/to/ocmini --mode steps --sizes 100 500 2000
python3 tests/stress_conversations.py /path/to/ocmini --mode archive --sizes 1000 10000 100000
python3 tests/stress_conversations.py /path/to/ocmini --mode search --sizes 1000 100005

JSON lines report process memory/CPU, latency, compactions and transcript integrity.
The provider checks context transport and tool pairing, not model recall quality.
"""

import argparse
import json
import math
import os
from pathlib import Path
import select
import socket
import sqlite3
import statistics
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen


GOAL = "GOAL_SENTINEL_preserve_this_constraint"
WINDOW = 8192


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def request(url, data=None):
    body = None if data is None else encode(data).encode()
    req = Request(url, data=body, headers={"Content-Type": "application/json"})
    with urlopen(req, timeout=120) as response:
        return json.load(response)


def resources(pid):
    status = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmHWM:", "Threads:")):
            key, value, *_ = line.split()
            status[key[:-1]] = int(value)
    stat = Path(f"/proc/{pid}/stat").read_text().split()
    status["cpu_seconds"] = (int(stat[13]) + int(stat[14])) / os.sysconf("SC_CLK_TCK")
    return status


class Fixture:
    def __init__(self, mode, steps):
        self.mode, self.steps = mode, steps
        self.rounds = self.summaries = self.overflows = self.max_request = 0
        self.expected_prompt = None
        self.errors = []

    def reply(self, data):
        serialized = encode(data)
        self.max_request = max(self.max_request, len(serialized.encode()))
        if "ARCHIVED_PAYLOAD" in serialized:
            self.errors.append("replayed archived payload")
        if not data.get("tools"):
            self.summaries += 1
            return {"content": f"{GOAL}; checkpoint {self.summaries}; retain progress."}, 100
        # The current user prompt can be kept verbatim outside the summarizer's
        # older-history input; it must be present in every actual task request.
        if GOAL not in serialized:
            self.errors.append("lost goal")
        if self.expected_prompt not in serialized:
            self.errors.append("lost current user prompt")
        # Every tool result must have exactly one preceding call in active context.
        pending = set()
        for message in data["messages"]:
            for call in message.get("tool_calls", []):
                if call["id"] in pending:
                    self.errors.append("duplicate tool call")
                pending.add(call["id"])
            if message["role"] == "tool":
                call = message["tool_call_id"]
                if call not in pending:
                    self.errors.append("orphan tool result")
                pending.discard(call)
        if pending:
            self.errors.append("missing tool result")
        used = max(1, len(serialized.encode()) // 4)
        if used > WINDOW:
            self.overflows += 1
            return None, used
        self.rounds += 1
        if self.mode == "search" and self.rounds <= 3:
            name, args = [
                ("grep", {"pattern": "SEARCH_TARGET_SENTINEL", "path": "files"}),
                ("glob", {"pattern": "*.txt", "path": "files"}),
                ("read", {"filePath": "large.txt", "offset": 2, "limit": 1}),
            ][self.rounds - 1]
            return {"tool_calls": [{"index": 0, "id": f"call_search_{name}", "type": "function",
                                     "function": {"name": name, "arguments": encode(args)}}]}, used
        if self.mode == "steps" and self.rounds <= self.steps:
            step = self.rounds
            args = {"todos": [{"content": f"Progress {step} " + "x" * 512,
                               "status": "in_progress", "priority": "medium"}]}
            return {"tool_calls": [{"index": 0, "id": f"call_{step}", "type": "function",
                                     "function": {"name": "todowrite", "arguments": encode(args)}}]}, used
        return {"content": f"Finished {self.rounds}; {GOAL}; " + "y" * 512}, used


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def log_message(self, *_):
        pass

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        delta, used = self.server.fixture.reply(data)
        if delta is None:
            body = encode({"error": {"message": f"maximum context length is {WINDOW} tokens"}}).encode()
            self.send_response(400)
        else:
            finish = "tool_calls" if "tool_calls" in delta else "stop"
            chunks = [
                {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                 "usage": {"prompt_tokens": used, "completion_tokens": 200}},
            ]
            body = ("".join(f"data: {encode(chunk)}\n\n" for chunk in chunks) + "data: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()


class Runtime:
    def __init__(self, binary, root, fixture):
        self.binary, self.root, self.fixture = binary, root, fixture
        self.project = root / "project"
        self.project.mkdir()
        self.provider = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.provider.daemon_threads = True
        self.provider.fixture = fixture
        threading.Thread(target=self.provider.serve_forever, daemon=True).start()
        self.env = {"PATH": os.environ.get("PATH", ""), "HOME": str(root),
                    "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "data"),
                    "OPENCODE_AUTH_CONTENT": "{}", "OPENCODE_DISABLE_PROJECT_CONFIG": "1"}
        config = {"model": "mock/test", "provider": {"mock": {
            "npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": f"http://127.0.0.1:{self.provider.server_port}/v1"},
            "models": {"test": {"name": "Fixture", "limit": {"context": WINDOW, "output": 1024}}}}}}
        self.env["OPENCODE_CONFIG_CONTENT"] = encode(config)
        self.db = root / "data/ocmini/ocmini.db"
        self.stderr = (root / "stderr").open("w+")
        self.start()
        self.sid = request(self.url + "/session", {"permission": [
            {"permission": "*", "pattern": "*", "action": "allow"}]})["id"]

    def start(self):
        self.process = subprocess.Popen([self.binary, "serve", "--port=0"], cwd=self.project,
                                        env=self.env, stdout=subprocess.PIPE, stderr=self.stderr,
                                        text=True)
        if not select.select([self.process.stdout], [], [], 15)[0]:
            raise RuntimeError("startup timed out")
        banner = self.process.stdout.readline().strip()
        assert banner.startswith("opencode server listening on "), banner
        self.url = banner.removeprefix("opencode server listening on ")

    def stop(self):
        self.process.terminate()
        self.process.wait(timeout=10)
        self.process.stdout.close()

    def close(self):
        self.stop()
        self.provider.shutdown()
        self.provider.server_close()
        self.stderr.close()

    def prompt(self, index):
        self.fixture.expected_prompt = f"{GOAL}; turn {index}; "
        body = {"messageID": f"msg_probe_{index}", "model": {"providerID": "mock", "modelID": "test"},
                "parts": [{"type": "text", "text": self.fixture.expected_prompt + "u" * 512}]}
        reply = request(self.url + f"/session/{self.sid}/message", body)
        assert "error" not in reply and not reply["info"].get("error"), reply
        assert any(p.get("text", "").startswith("Finished") for p in reply["parts"]), reply

    def seed(self, count):
        with sqlite3.connect(self.db) as db:
            def add(mid, role, tick, text, summary=False):
                info = {"id": mid, "sessionID": self.sid, "role": role,
                        "time": {"created": tick, "completed": tick},
                        "providerID": "mock", "modelID": "test"}
                if role == "assistant":
                    info["parentID"] = "msg_seed_user"
                if summary:
                    info["summary"] = True
                part = {"id": "prt_" + mid, "sessionID": self.sid, "messageID": mid,
                        "type": "text", "text": text}
                db.execute("INSERT INTO message VALUES(?,?,?,?,?)", (mid, self.sid, role, encode(info), tick))
                db.execute("INSERT INTO part VALUES(?,?,?,?,?)", (part["id"], mid, self.sid, encode(part), tick))
            add("msg_seed_user", "user", 1, GOAL)
            for i in range(count):
                add(f"msg_seed_{i}", "assistant", i + 2, "ARCHIVED_PAYLOAD" + "a" * 3072)
            add("msg_checkpoint", "assistant", count + 2, GOAL + "; checkpoint", summary=True)

    def seed_search(self, count):
        directory = self.project / "files"
        directory.mkdir()
        for index in range(count):
            (directory / f"{index:06}.txt").touch()
        (directory / f"{count-1:06}.txt").write_text("SEARCH_TARGET_SENTINEL\n")
        with (self.project / "large.txt").open("wb") as file:
            chunk = b"x" * (1024 * 1024)
            for _ in range(33):
                file.write(chunk)
            file.write(b"\nLAST_LINE\n")


def run(binary, mode, sizes):
    # Turn checkpoints share one session; archive and single-task cases use fresh processes.
    groups = [sizes] if mode == "turns" else [[size] for size in sizes]
    for group in groups:
        with tempfile.TemporaryDirectory(prefix="ocmini-stress-") as directory:
            fixture = Fixture(mode, group[-1])
            runtime = Runtime(binary, Path(directory), fixture)
            previous = 0
            try:
                if mode == "archive":
                    runtime.seed(group[0])
                if mode == "search":
                    runtime.seed_search(group[0])
                for size in group:
                    before = resources(runtime.process.pid)
                    started = time.monotonic()
                    timings = []
                    count = size - previous if mode == "turns" else (20 if mode == "archive" else 1)
                    for i in range(count):
                        tick = time.monotonic()
                        runtime.prompt(previous + i)
                        timings.append((time.monotonic() - tick) * 1000)
                    after = resources(runtime.process.pid)
                    with sqlite3.connect(runtime.db) as db:
                        messages = db.execute("SELECT count(*) FROM message WHERE session_id=?", (runtime.sid,)).fetchone()[0]
                        tools = db.execute("SELECT count(*) FROM part WHERE session_id=? AND json_extract(json,'$.type')='tool' AND json_extract(json,'$.state.status')='completed'", (runtime.sid,)).fetchone()[0]
                        summaries = db.execute("SELECT count(*) FROM message WHERE session_id=? AND json_extract(json,'$.summary')=1", (runtime.sid,)).fetchone()[0]
                        if mode == "search":
                            results = {json.loads(row[0])["tool"]: json.loads(row[0])["state"] for row in db.execute(
                                "SELECT json FROM part WHERE session_id=? AND json_extract(json,'$.type')='tool'", (runtime.sid,))}
                            assert all(result["status"] == "completed" for result in results.values()), results
                            assert results["grep"]["metadata"]["matches"] == 1
                            assert "SEARCH_TARGET_SENTINEL" in results["grep"]["output"]
                            assert results["glob"]["metadata"]["matches"] == 100
                            assert results["glob"]["metadata"]["truncated"] is True
                            assert results["read"]["output"] == "2: LAST_LINE\n"
                    assert not fixture.errors, fixture.errors[:5]
                    if mode == "steps":
                        assert tools == size, (tools, size)
                    if mode == "search":
                        assert tools == 3, tools
                    if mode == "archive":
                        # Output-aware headroom may create more checkpoints
                        # during these turns; those are additional saved messages.
                        assert messages == size + 2 + 2 * count + summaries - 1, messages
                        with sqlite3.connect(runtime.db) as db:
                            archived = db.execute("SELECT count(*) FROM message WHERE session_id=? AND id LIKE 'msg_seed_%'", (runtime.sid,)).fetchone()[0]
                            assert archived == size + 1, archived
                    print(encode({"mode": mode, "size": size, "elapsed_s": round(time.monotonic()-started, 3),
                                  "cpu_s": round(after["cpu_seconds"]-before["cpu_seconds"], 3),
                                  "rss_mib": round(after["VmRSS"]/1024, 2), "peak_mib": round(after["VmHWM"]/1024, 2),
                                  "median_ms": round(statistics.median(timings), 3),
                                  "p95_ms": round(sorted(timings)[math.ceil(len(timings)*.95)-1], 3),
                                  "messages": messages, "tools": tools, "summaries": summaries,
                                  "overflows": fixture.overflows, "max_request_bytes": fixture.max_request}), flush=True)
                    previous = size
                runtime.stop()
                runtime.start()
                runtime.fixture.mode = "turns"
                runtime.prompt(previous + 1)
                assert not fixture.errors, fixture.errors[:5]
            except Exception as error:
                print(encode({"mode": mode, "size": group[-1], "failed": str(error),
                              "completed_model_rounds": fixture.rounds, "summaries": fixture.summaries,
                              "overflows": fixture.overflows, "context_errors": fixture.errors[:5]}), flush=True)
                raise
            finally:
                runtime.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda path: str(Path(path).resolve()))
    parser.add_argument("--mode", choices=["turns", "steps", "archive", "search"], required=True)
    parser.add_argument("--sizes", nargs="+", type=int, required=True)
    args = parser.parse_args()
    assert all(size > 0 for size in args.sizes) and args.sizes == sorted(set(args.sizes))
    run(args.binary, args.mode, args.sizes)
