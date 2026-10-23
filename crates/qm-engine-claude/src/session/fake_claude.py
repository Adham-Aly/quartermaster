# A fake `claude` for the session tests: answers the control protocol and plays a scenario.
#
# Usage: fake_claude.py <scenario.json> <record file>
#
# The scenario is {"on_initialize": [line, ...], "turns": [[step, ...], ...]}. Each user message
# plays the next list of steps on its own thread; a step is {"line": <message>} (written as one
# JSON line), {"sleep_ms": n} or {"exit": code}. Every line read on stdin is appended to the record
# file. `initialize` and `interrupt` are answered with success, any other control request with an
# error, the way an older claude refuses a query it doesn't know.
import json
import os
import sys
import threading
import time

scenario = json.load(open(sys.argv[1]))
record = open(sys.argv[2], "a")
lock = threading.Lock()


def write(msg):
    with lock:
        sys.stdout.write(json.dumps(msg) + "\n")
        sys.stdout.flush()


def play(steps):
    for step in steps:
        if "sleep_ms" in step:
            time.sleep(step["sleep_ms"] / 1000)
        elif "exit" in step:
            sys.stdout.flush()
            os._exit(step["exit"])
        else:
            write(step["line"])


turns = list(scenario.get("turns", []))
for raw in sys.stdin:
    record.write(raw)
    record.flush()
    msg = json.loads(raw)
    kind = msg.get("type")
    if kind == "control_request":
        request = msg.get("request", {})
        subtype = request.get("subtype")
        if subtype == "initialize":
            play(scenario.get("on_initialize", []))
        body = {"request_id": msg["request_id"]}
        if subtype in ("initialize", "interrupt"):
            body.update({"subtype": "success", "response": {}})
        else:
            body.update({"subtype": "error", "error": "unknown request"})
        write({"type": "control_response", "response": body})
    elif kind == "user" and turns:
        threading.Thread(target=play, args=(turns.pop(0),), daemon=True).start()
