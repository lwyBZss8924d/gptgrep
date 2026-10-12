"""Synthetic consumer used as the supervised command in lifecycle tests.

Modes (argv[1]):
  env-report OUT                 write env NAMES plus selected non-secret values
  hold SECONDS READY LEASE       verify the inherited lease fd is LEASE, touch READY, sleep
  exit CODE                      exit with CODE
  ignore-term SECONDS READY      ignore SIGTERM and sleep
  survivor SECONDS keep|close    fork a grandchild that outlives this process;
                                 'close' also calls setsid() and closes inherited fds
  write-canonical                append to $CODEX_HOME/auth.json through the run's link
                                 (a misbehaving consumer rewriting the fake credential)
Never prints environment values.
"""
import json
import os
import signal
import sys
import time

REPORTED_VALUES = ("CODEX_HOME", "CODEX_SQLITE_HOME", "CODEX_REFRESH_TOKEN_URL_OVERRIDE", "HOME", "TMPDIR",
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


def main(argv):
    mode = argv[1]
    if mode == "env-report":
        values = {k: os.environ.get(k) for k in REPORTED_VALUES if k in os.environ}
        canary = any("FAKE-CANARY" in v for v in os.environ.values())
        report = {"names": sorted(os.environ), "values": values, "canary_value_present": canary,
                  "openrouter_present": "OPENROUTER_API_KEY" in os.environ, "cwd": os.getcwd(),
                  "lease_fd_open": lease_fd_matches()}
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
    if mode == "write-canonical":
        path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
        fd = os.open(path, os.O_WRONLY | os.O_APPEND)
        os.write(fd, b"\n")
        os.close(fd)
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
