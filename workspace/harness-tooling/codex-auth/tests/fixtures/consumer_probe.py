"""Synthetic consumer used as the supervised command in lifecycle tests.

Modes (argv[1]):
  env-report OUT                 write env NAMES plus selected non-secret values
  hold SECONDS READY LEASE       verify the inherited lease fd is LEASE, touch READY, sleep
  exit CODE                      exit with CODE (77 is the BLOCKED_AUTH contract)
  ignore-term SECONDS READY      ignore SIGTERM and sleep
  survivor SECONDS keep|close    fork a grandchild that outlives this process;
                                 'close' also calls setsid() and closes inherited fds
  escaped PIDFILE SECONDS [ACTION ARGS...]
                                 fork a setsid grandchild with closed fds (cwd stays in
                                 the run's work dir), write its pid, return. After
                                 SECONDS the grandchild runs ACTION, then exits:
                                   canonical-append      append to $CODEX_HOME/auth.json
                                   docker-add STATE ID   add a fake container id, sleep 1
  chmod-home PIDFILE SECONDS     an escaped grandchild as above, then chmod $CODEX_HOME 000
  write-canonical                append to $CODEX_HOME/auth.json through the run's link
                                 (a misbehaving consumer rewriting the fake credential)
  replace-link                   replace the run's auth.json link with a regular file
                                 holding a synthetic credential document (a per-run login)
  bind-port PORT SECONDS READY   listen on 127.0.0.1:PORT (a would-be token harvester)
  root-first-cleanup DIR         fork a grandchild; on SIGTERM check the grandchild was
                                 not signalled yet, record it in DIR, stop it, exit 0
Never prints environment values.
"""
import json
import os
import signal
import socket
import sys
import time

REPORTED_VALUES = ("CODEX_HOME", "CODEX_SQLITE_HOME", "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
                   "CODEX_REVOKE_TOKEN_URL_OVERRIDE", "HOME", "TMPDIR",
                   "NO_PROXY", "no_proxy", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                   "GPTGREP_AUTH_RUN_ID", "CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED", "PATH", "LANG",
                   "LC_ALL", "DOCKER_HOST")


def lease_fd_matches(lease_path=None):
    """True when GPTGREP_AUTH_LEASE_FD is an open descriptor (of LEASE_PATH, if given)."""
    fd = os.environ.get("GPTGREP_AUTH_LEASE_FD")
    if fd is None:
        return None
    try:
        held = os.fstat(int(fd))
    except OSError:
        return False
    if lease_path is None:
        return True
    target = os.stat(lease_path)
    return (held.st_dev, held.st_ino) == (target.st_dev, target.st_ino)


def auth_link():
    path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
    return {"is_symlink": os.path.islink(path), "target": os.readlink(path) if os.path.islink(path) else None}


def escape(pidfile, seconds, action=None, args=()):
    """Fork a daemon-like grandchild that the supervisor must still find."""
    home = os.environ.get("CODEX_HOME")
    pid = os.fork()
    if pid == 0:
        os.setsid()
        with open(pidfile, "w") as handle:
            handle.write(str(os.getpid()))
        for fd in range(0, 256):
            try:
                os.close(fd)
            except OSError:
                pass
        time.sleep(seconds)
        try:
            if action == "canonical-append":
                fd = os.open(os.path.join(home, "auth.json"), os.O_WRONLY | os.O_APPEND)
                os.write(fd, b"\n")
                os.close(fd)
            elif action == "docker-add":
                with open(args[0], "a") as handle:
                    handle.write("\n" + args[1] + "\n")
                time.sleep(1.0)
        finally:
            os._exit(0)
    deadline = time.time() + 10
    while not os.path.exists(pidfile) and time.time() < deadline:
        time.sleep(0.05)
    time.sleep(1.0)  # let the supervisor's tracker sample the grandchild


def main(argv):
    mode = argv[1]
    if mode == "env-report":
        values = {k: os.environ.get(k) for k in REPORTED_VALUES if k in os.environ}
        canary = any("FAKE-CANARY" in v for v in os.environ.values())
        report = {"names": sorted(os.environ), "values": values, "canary_value_present": canary,
                  "openrouter_present": "OPENROUTER_API_KEY" in os.environ, "cwd": os.getcwd(),
                  "lease_fd_open": lease_fd_matches(), "auth_link": auth_link()}
        with open(argv[2], "w") as handle:
            json.dump(report, handle)
        return 0
    if mode == "hold":
        seconds, ready = float(argv[2]), argv[3]
        if not lease_fd_matches(argv[4] if len(argv) > 4 else None):
            return 9
        open(ready, "w").close()
        time.sleep(seconds)
        return 0
    if mode == "exit":
        return int(argv[2])
    if mode == "ignore-term":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        open(argv[3], "w").close()
        time.sleep(float(argv[2]))
        return 0
    if mode == "survivor":
        seconds, how = float(argv[2]), argv[3]
        pid = os.fork()
        if pid == 0:
            if how == "close":
                os.setsid()
                for fd in range(0, 256):
                    try:
                        os.close(fd)
                    except OSError:
                        pass
            time.sleep(seconds)
            os._exit(0)
        return 0
    if mode == "escaped":
        escape(argv[2], float(argv[3]), argv[4] if len(argv) > 4 else None, argv[5:])
        return 0
    if mode == "chmod-home":
        escape(argv[2], float(argv[3]))
        os.chmod(os.environ["CODEX_HOME"], 0)
        return 0
    if mode == "write-canonical":
        path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
        fd = os.open(path, os.O_WRONLY | os.O_APPEND)
        os.write(fd, b"\n")
        os.close(fd)
        return 0
    if mode == "replace-link":
        path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
        os.unlink(path)
        document = {"auth_mode": "chatgpt", "tokens": {"refresh_token": "FAKE-CANARY-PER-RUN-LOGIN"}}
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, json.dumps(document).encode())
        os.close(fd)
        return 0
    if mode == "bind-port":
        port, seconds, ready = int(argv[2]), float(argv[3]), argv[4]
        with socket.socket() as server:
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            server.bind(("127.0.0.1", port))
            server.listen(4)
            open(ready, "w").close()
            time.sleep(seconds)
        return 0
    if mode == "root-first-cleanup":
        directory = argv[2]
        child = os.fork()
        if child == 0:
            signal.signal(signal.SIGTERM, lambda *_: os._exit(15))
            time.sleep(120)
            os._exit(0)

        def on_term(*_args):
            time.sleep(1.0)
            try:
                finished, _status = os.waitpid(child, os.WNOHANG)
            except ChildProcessError:
                finished = child
            with open(os.path.join(directory, "root-cleanup.json"), "w") as handle:
                json.dump({"grandchild_alive_during_root_cleanup": finished == 0}, handle)
            os.kill(child, signal.SIGKILL)
            os.waitpid(child, 0)
            os._exit(0)

        signal.signal(signal.SIGTERM, on_term)
        open(os.path.join(directory, "root-ready"), "w").close()
        time.sleep(120)
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
