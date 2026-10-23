#!/usr/bin/env python3
"""Offline resource profile of the real BOR Rust polling process on Linux.

Never contacts Telegram. Requires cargo build --release --features resource-bench.
Only samples /proc of the child process; does not expose BotFather secrets.
"""
import argparse
import http.server
import json
import os
import pathlib
import statistics
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlparse

class FakeTelegram(http.server.ThreadingHTTPServer):
    daemon_threads = True
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.lock = threading.Lock()
        self.events = []
        self.counter = 0
        self.replied = 0
        self.sent = 0
        self.requests = 0
        self.last_update = 0

    def push(self, count):
        now = int(time.time())
        with self.lock:
            for _ in range(count):
                self.counter += 1
                uid = self.counter + 10000
                self.last_update += 1
                self.events.append({
                    "update_id": self.last_update,
                    "chat_join_request": {
                        "chat": {"id": -1005551}, "from": {"id": uid, "is_bot": False},
                        "user_chat_id": uid, "date": now
                    }
                })

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_POST(self):
        name = urlparse(self.path).path.rsplit("/", 1)[-1]
        size = int(self.headers.get("Content-Length", 0))
        data = json.loads(self.rfile.read(size) or b"{}")
        srv = self.server
        with srv.lock:
            srv.requests += 1
            if name == "getUpdates":
                offset = data.get("offset", 0)
                batch = [x for x in srv.events if x["update_id"] >= offset][:50]
                srv.events = [x for x in srv.events if x["update_id"] >= offset + len(batch)]
                result = batch
            elif name == "getMe":
                result = {"id": 42, "username": "BenchBot", "is_bot": True}
            elif name == "getWebhookInfo":
                result = {"url": ""}
            elif name == "getChatMember":
                result = {"status": "left"}
            elif name == "sendMessage":
                srv.sent += 1
                result = {"message_id": srv.sent}
            elif name == "answerCallbackQuery":
                srv.replied += 1
                result = True
            else:
                result = True
        body = json.dumps({"ok": True, "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

def proc_stats(pid):
    status = pathlib.Path(f"/proc/{pid}/status").read_text()
    values = {}
    for line in status.splitlines():
        if line.startswith(("VmRSS:", "VmHWM:", "VmSize:", "Threads:")):
            key, value = line.split(":", 1)
            values[key] = int(value.split()[0])
    stat = pathlib.Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    values["cpu_ticks"] = int(stat[11]) + int(stat[12])
    return values

def measure(proc, seconds, interval):
    rows = []
    start = time.monotonic()
    while time.monotonic() - start < seconds:
        if proc.poll() is not None:
            raise RuntimeError(f"bot exited early ({proc.returncode})")
        rows.append(proc_stats(proc.pid))
        time.sleep(interval)
    return rows

def summarize(rows, elapsed):
    clock = os.sysconf("SC_CLK_TCK")
    return {
        "rss_mib_avg": round(statistics.mean(row.get("VmRSS", 0) for row in rows) / 1024, 3),
        "rss_mib_peak": round(max(row.get("VmHWM", 0) for row in rows) / 1024, 3),
        "virtual_mib_peak": round(max(row.get("VmSize", 0) for row in rows) / 1024, 3),
        "threads_peak": max(row.get("Threads", 0) for row in rows),
        "cpu_core_percent": round((rows[-1]["cpu_ticks"] - rows[0]["cpu_ticks"]) / clock / elapsed * 100, 2),
    }

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--events", type=int, default=200)
    parser.add_argument("--seconds", type=float, default=8.0)
    parser.add_argument("--output", default="bor-resource-report.json")
    args = parser.parse_args()
    assert args.events > 0 and args.seconds >= 2
    server = FakeTelegram()
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    with tempfile.TemporaryDirectory(prefix="bor-bench-") as directory:
        env = dict(os.environ, BOT_TOKEN="BENCH_ONLY_NEVER_REAL",
                   DATABASE_PATH=os.path.join(directory, "bot.sqlite3"),
                   BOR_BENCH_ENDPOINT=f"http://127.0.0.1:{server.server_port}/botMOCK/",
                   RUST_LOG="error")
        proc = subprocess.Popen([args.binary], env=env, stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE, text=True)
        try:
            idle_start = time.monotonic()
            idle = measure(proc, args.seconds, .1)
            idle_elapsed = time.monotonic() - idle_start
            server.push(args.events)
            load_start = time.monotonic()
            load = measure(proc, args.seconds, .1)
            load_elapsed = time.monotonic() - load_start
            with server.lock:
                completed = server.sent
                calls = server.requests
                remaining = len(server.events)
            report = {
                "runner": "Linux /proc (child PID only, not Docker overhead)",
                "scenario": "join_request -> SQLite -> sendMessage mock; no real Telegram API",
                "events_requested": args.events, "captchas_delivered": completed,
                "events_remaining_in_mock_queue": remaining,
                "mock_api_calls": calls,
                "idle": summarize(idle, idle_elapsed),
                "load": summarize(load, load_elapsed),
                "db_size_bytes": sum(
                    p.stat().st_size for p in pathlib.Path(directory).glob("bot.sqlite3*")
                    if p.is_file()
                ),
                "warning": "CPU includes wall-clock phases, load may continue beyond sample; sampled VmHWM is process high-water RSS."
            }
            if completed < args.events:
                report["incomplete"] = True
            pathlib.Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            print(json.dumps(report, ensure_ascii=False, indent=2))
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            server.shutdown()

if __name__ == "__main__":
    main()
