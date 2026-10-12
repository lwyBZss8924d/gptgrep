"""Fake `codex` binary for owner and re-login unit tests (never the real CLI).

Behaviour comes from `<this file>.json` (the supervisor passes an allowlisted
environment, so no mode variable can be inherited):
  {"app_server": "normal" | "no_write" | "slow_term" | "stuck" | "permanent_fail" | "transient_fail" | "slow_refresh",
   "login_exit": 0, "status_exit": "auto" | int, "login_sleep": 0.0,
   "refresh_delay": 0.0, "started": PATH, "finished": PATH}

app-server: newline JSON-RPC on stdin/stdout. account/read with refreshToken
rewrites $CODEX_HOME/auth.json IN PLACE through the run's symlink (like the
official FileAuthStorage::save) unless the mode is no_write or *_fail. The
*_fail modes print the official 0.162.1 log line shapes to stderr
(codex_login / account_processor targets) and, for permanent_fail, return a
null account like the official app-server after a permanent refresh failure.
slow_refresh touches `started`, sleeps `refresh_delay`, rewrites, touches `finished`.
login --device-auth: like the official CLI, first revokes and deletes an
existing $CODEX_HOME/auth.json (clear_existing_auth_before_login; recorded as
"cleared_existing_auth"), then prints a fake device code, then, on success,
creates a new auth.json.
login status: "auto" exits 0 when $CODEX_HOME/auth.json exists and is non-empty.
exec: reads the prompt from stdin when the last argument is "-", writes a fake
verdict to the `-o` path and exits 0 (no model; the Judge argv shape only).
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

PERMANENT_LINE = ('\x1b[2m2026-10-12T00:00:00.000000Z\x1b[0m \x1b[31mERROR\x1b[0m codex_login::auth::manager: '
                  'Failed to refresh token status=401 Unauthorized detail=TokenErrorDetail { error_code: '
                  'Some("refresh_token_reused"), error_message: Some("fake"), .. }')
TRANSIENT_LINE = ('2026-10-12T00:00:00.000000Z  WARN codex_app_server::request_processors::account_processor: '
                  'failed to refresh token while getting account: error sending request for url '
                  '(http://127.0.0.1:1/oauth/token)')


def record(kind, **extra):
    path = os.path.join(os.environ.get("CODEX_SQLITE_HOME", "/nonexistent"), "fake-codex-record.jsonl")
    try:
        with open(path, "a") as handle:
            handle.write(json.dumps(dict(extra, kind=kind, argv=sys.argv[1:], env_names=sorted(os.environ),
                                         codex_home=os.environ.get("CODEX_HOME"),
                                         refresh_override=os.environ.get("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
                                         revoke_override=os.environ.get("CODEX_REVOKE_TOKEN_URL_OVERRIDE"),
                                         rust_log=os.environ.get("RUST_LOG"))) + "\n")
    except OSError:
        pass


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def touch(path):
    if path:
        open(path, "w").close()


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
        account = {"type": "chatgpt", "email": "fake@example.invalid", "planType": "plus"}
        if method == "initialize":
            send({"id": message["id"], "result": {"userAgent": "fake", "codexHome": os.environ.get("CODEX_HOME")}})
        elif method == "account/read":
            refresh = (message.get("params") or {}).get("refreshToken")
            if refresh and mode == "permanent_fail":
                sys.stderr.write(PERMANENT_LINE + "\n")
                sys.stderr.flush()
            elif refresh and mode == "transient_fail":
                sys.stderr.write(TRANSIENT_LINE + "\n")
                sys.stderr.flush()
            elif refresh and mode not in ("no_write",):
                if mode == "slow_refresh":
                    touch(SETTINGS.get("started"))
                    time.sleep(float(SETTINGS.get("refresh_delay", 0)))
                generation += 1
                rewrite_in_place(generation)
                if mode == "slow_refresh":
                    touch(SETTINGS.get("finished"))
            if mode == "permanent_fail":
                account = None
            send({"id": message["id"], "result": {"account": account, "requiresOpenaiAuth": True}})
        else:
            send({"id": message["id"], "error": {"code": -32601, "message": "fake: unsupported"}})
    if mode in ("slow_term", "stuck"):
        while True:
            time.sleep(0.2)
    return 0


def login():
    args = sys.argv[1:]
    path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
    if "--device-auth" in args:
        cleared = os.path.lexists(path)
        if cleared:
            os.unlink(path)  # the official CLI revokes the refresh token, then deletes the file
        record("login", cleared_existing_auth=cleared)
        print("FAKE-DEVICE-CODE-7Q2W-9ZXC (synthetic; visit the official device page)", flush=True)
        time.sleep(float(SETTINGS.get("login_sleep", 0)))
        code = int(SETTINGS.get("login_exit", 0))
        if code == 0:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.write(fd, b'{"auth_mode": "chatgpt", "tokens": {"refresh_token": "FAKE-RELOGIN-RT"}}')
            os.close(fd)
        return code
    record("status")
    status = SETTINGS.get("status_exit", "auto")
    if status == "auto":
        ok = os.path.exists(path) and os.path.getsize(path) > 0
        print("Logged in using ChatGPT (fake)" if ok else "Not logged in (fake)", file=sys.stderr, flush=True)
        return 0 if ok else 1
    return int(status)


def exec_():
    args = sys.argv[1:]
    prompt = sys.stdin.read() if args and args[-1] == "-" else ""
    record("exec", stdin_bytes=len(prompt))
    if "-o" in args:
        with open(args[args.index("-o") + 1], "w") as handle:
            json.dump({"verdict": "fake", "stdin_bytes": len(prompt)}, handle)
    return 0


def main():
    args = sys.argv[1:]
    if "app-server" in args:
        return app_server()
    if "login" in args:
        return login()
    if "exec" in args:
        return exec_()
    if "--version" in args:
        print("codex-cli 0.0.0-fake")
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
