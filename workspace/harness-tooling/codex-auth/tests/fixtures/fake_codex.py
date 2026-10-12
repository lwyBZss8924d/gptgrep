"""Fake `codex` binary for owner and re-login unit tests (never the real CLI).

Behaviour comes from `<this file>.json` (the supervisor passes an allowlisted
environment, so no mode variable can be inherited):
  {"app_server": "normal" | "no_write" | "slow_term" | "stuck",
   "login_exit": 0, "status_exit": 0, "login_sleep": 0.0}

app-server: newline JSON-RPC on stdin/stdout. account/read with refreshToken
rewrites $CODEX_HOME/auth.json IN PLACE through the run's symlink (like the
official FileAuthStorage::save) unless mode is no_write.
login --device-auth: prints a fake device code to the terminal, then replaces
$CODEX_HOME/auth.json (unlink + create, like logout-then-login).
Every invocation appends argv and environment NAMES to $CODEX_SQLITE_HOME/fake-codex-record.jsonl.
"""
import json
import os
import signal
import sys
import time

SELF = os.path.realpath(sys.argv[0])
with open(SELF + ".json") as handle:
    SETTINGS = json.load(handle)


def record(kind):
    path = os.path.join(os.environ.get("CODEX_SQLITE_HOME", "/nonexistent"), "fake-codex-record.jsonl")
    try:
        with open(path, "a") as handle:
            handle.write(json.dumps({"kind": kind, "argv": sys.argv[1:], "env_names": sorted(os.environ),
                                     "codex_home": os.environ.get("CODEX_HOME"),
                                     "refresh_override": os.environ.get("CODEX_REFRESH_TOKEN_URL_OVERRIDE")}) + "\n")
    except OSError:
        pass


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def rewrite_in_place(generation):
    path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
    document = {"auth_mode": "chatgpt", "OPENAI_API_KEY": None,
                "tokens": {"id_token": "FAKE.OWNER.ID%d" % generation, "access_token": "FAKE.OWNER.AT%d" % generation,
                           "refresh_token": "FAKE-OWNER-RT-%d" % generation, "account_id": "acct-fake-0001"},
                "last_refresh": "2026-10-12T00:00:00Z"}
    fd = os.open(path, os.O_WRONLY | os.O_TRUNC)
    try:
        os.write(fd, json.dumps(document, indent=2).encode())
    finally:
        os.close(fd)


def app_server():
    mode = SETTINGS.get("app_server", "normal")
    record("app-server")
    if mode == "slow_term":
        signal.signal(signal.SIGTERM, lambda *_: (time.sleep(1.0), os._exit(0)))
    if mode == "stuck":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    generation = 0
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except ValueError:
            continue
        method = message.get("method")
        if "id" not in message:
            continue
        if method == "initialize":
            send({"id": message["id"], "result": {"userAgent": "fake", "codexHome": os.environ.get("CODEX_HOME")}})
        elif method == "account/read":
            if (message.get("params") or {}).get("refreshToken") and mode != "no_write":
                generation += 1
                rewrite_in_place(generation)
            send({"id": message["id"], "result": {"account": {"type": "chatgpt", "email": "fake@example.invalid",
                                                              "planType": "plus"}, "requiresOpenaiAuth": True}})
        else:
            send({"id": message["id"], "error": {"code": -32601, "message": "fake: unsupported"}})
    if mode in ("slow_term", "stuck"):
        while True:
            time.sleep(0.2)
    return 0


def login():
    args = sys.argv[1:]
    record("login" if "--device-auth" in args else "status")
    if "--device-auth" in args:
        print("FAKE-DEVICE-CODE-7Q2W-9ZXC (synthetic; visit the official device page)", flush=True)
        time.sleep(float(SETTINGS.get("login_sleep", 0)))
        code = int(SETTINGS.get("login_exit", 0))
        if code == 0:
            path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
            if os.path.lexists(path):
                os.unlink(path)
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.write(fd, b'{"auth_mode": "chatgpt", "tokens": {"refresh_token": "FAKE-RELOGIN-RT"}}')
            os.close(fd)
        return code
    print("Logged in using ChatGPT (fake)", flush=True)
    return int(SETTINGS.get("status_exit", 0))


def main():
    args = sys.argv[1:]
    if "app-server" in args:
        return app_server()
    if "login" in args:
        return login()
    if "--version" in args:
        print("codex-cli 0.0.0-fake")
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
