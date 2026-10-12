"""Consumer-side JSON-RPC driver for `codex app-server --listen stdio://`.

Runs as the supervised command, inheriting the supervisor-built environment
(per-run CODEX_HOME, refresh blocker, allowlist). Executes a JSON list of steps
and writes only non-secret classifications (no emails, tokens or bodies).

usage: rpc_client.py TOOL_DIR CODEX_BIN STEPS_JSON OUT_JSON STDERR_LOG
step ops: {"op": "request", "method": M, "params": P, "timeout": S}
          {"op": "notify", "method": M}
          {"op": "sleep", "seconds": S}
          {"op": "touch", "path": P}
          {"op": "wait_file", "path": P, "timeout": S}
"""
import json
import os
import subprocess
import sys
import time


def main(argv):
    tool_dir, codex_bin, steps_path, out_path, stderr_path = argv[1:6]
    sys.path.insert(0, tool_dir)
    import codex_auth_supervisor as sup

    with open(steps_path) as handle:
        steps = json.load(handle)
    with open(stderr_path, "wb") as stderr_log:
        proc = subprocess.Popen([codex_bin, "app-server", "--listen", "stdio://", "--strict-config"],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr_log)
    client = sup.RpcClient(proc)
    results = []
    for step in steps:
        op = step["op"]
        try:
            if op == "request":
                message = client.request(step["method"], step.get("params"), step.get("timeout", 60))
                item = sup.classify_response(step["method"], message)
                if step["method"] == "thread/start":
                    item["thread_started"] = bool((message.get("result") or {}).get("thread"))
                item["at"] = time.time()
                results.append(item)
            elif op == "notify":
                client.notify(step["method"], step.get("params"))
            elif op == "sleep":
                time.sleep(step["seconds"])
            elif op == "touch":
                open(step["path"], "w").close()
            elif op == "wait_file":
                deadline = time.monotonic() + step.get("timeout", 30)
                while not os.path.exists(step["path"]) and time.monotonic() < deadline:
                    time.sleep(0.05)
                results.append({"op": "wait_file", "ok": os.path.exists(step["path"])})
        except sup.RpcError as exc:
            results.append({"op": op, "method": step.get("method"), "ok": False, "error": str(exc)})
            break
    client.close_stdin()
    try:
        code = proc.wait(timeout=20)
    except subprocess.TimeoutExpired:
        proc.terminate()
        code = proc.wait(timeout=20)
    with open(out_path, "w") as handle:
        json.dump({"results": results, "app_server_exit": code, "notifications": client.notifications[:50]},
                  handle)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
