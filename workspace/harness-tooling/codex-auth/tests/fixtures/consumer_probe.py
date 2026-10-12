"""Synthetic consumer used as the supervised command in lifecycle tests.

Modes (argv[1]):
  env-report OUT [LEASE]         write env NAMES plus selected non-secret values and the
                                 number of inherited descriptors of LEASE
  hold SECONDS READY [LEASE]     verify no descriptor of LEASE was inherited, touch READY, sleep
  unlock-lease LEASE SECONDS READY REPORT
                                 a consumer trying to release the run's lease: flock(LOCK_UN)
                                 on every inherited descriptor of LEASE and on a fresh one,
                                 write REPORT, touch READY, sleep
  exit CODE                      exit with CODE (77 alone is not an auth failure)
  auth-fail CLASS                write the auth-failure record ($GPTGREP_AUTH_FAILURE_RECORD)
                                 with CLASS, then exit 77 (the BLOCKED_AUTH contract)
  auth-record TEXT CODE          write TEXT verbatim as the auth-failure record, exit CODE
  auth-record-symlink TARGET CODE
                                 make the auth-failure record a symlink to TARGET, exit CODE
  link-credential KIND NAME      link the credential into the run's results directory as NAME:
                                 hard (a hard link to the canonical file), sym-canonical (a
                                 symlink to it), sym-run-link (a relative symlink to the run's
                                 auth.json link), sym-home (a symlink to the canonical auth
                                 home); hard-outside PATH links it at PATH instead
  move-canonical-keep NAME       hard-link the credential into results as NAME, then replace
                                 the canonical path with another file (NAME is then the only
                                 link to the original credential)
  deep-link LEVELS SEGMENT KIND=NAME...
                                 below results/deep, create LEVELS nested directories named
                                 SEGMENT (relative steps, so the path may exceed PATH_MAX) and
                                 in the deepest one a symlink NAME per KIND: canonical (to the
                                 canonical file), home (to the auth home) or root (to /);
                                 write the deepest directory's path length to
                                 results/deep-path-length.txt
  hidden-link NAME               symlink results/hide/NAME to the credential, then chmod
                                 results/hide 000 (a directory the scan cannot read)
  move-home NAME                 move the canonical auth home into results as NAME, leave a
                                 symlink to it at the auth home's path and add a second hard
                                 link NAME/auth2.json to the credential
  symlink TARGET LINK            create the symlink LINK -> TARGET
  ignore-term SECONDS READY      ignore SIGTERM and sleep
  survivor SECONDS keep|close    fork a grandchild that outlives this process;
                                 'close' also calls setsid() and closes inherited fds
  escaped PIDFILE SECONDS [ACTION ARGS...]
                                 fork a setsid grandchild with closed fds (cwd stays in
                                 the run's work dir), write its pid, return. After
                                 SECONDS the grandchild runs ACTION, then exits:
                                   canonical-append      append to $CODEX_HOME/auth.json
                                   docker-add STATE ID   add a fake container id, sleep 1
                                   hard-link NAME        hard-link the credential into results
                                   hidden-link NAME      as the hidden-link mode
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
import fcntl
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
                   "LC_ALL", "DOCKER_HOST", "GPTGREP_AUTH_FAILURE_RECORD")


def lease_descriptors(lease_path):
    """Inherited descriptors that refer to the lease file (none since supervisor 1.3.0)."""
    target = os.stat(lease_path)
    found = []
    for fd in range(3, 256):
        try:
            info = os.fstat(fd)
        except OSError:
            continue
        if (info.st_dev, info.st_ino) == (target.st_dev, target.st_ino):
            found.append(fd)
    return found


def auth_link():
    path = os.path.join(os.environ["CODEX_HOME"], "auth.json")
    return {"is_symlink": os.path.islink(path), "target": os.readlink(path) if os.path.islink(path) else None}


def results_dir():
    return os.path.join(os.path.dirname(os.environ["CODEX_HOME"]), "results")


def canonical_path():
    """The canonical credential path, read from the run's link (readlink only)."""
    return os.readlink(os.path.join(os.environ["CODEX_HOME"], "auth.json"))


def write_record(text):
    fd = os.open(os.environ["GPTGREP_AUTH_FAILURE_RECORD"], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(fd, text.encode())
    os.close(fd)


def hidden_link(canonical, results, name):
    """A symlink to the credential in a directory the post-run scan cannot read."""
    hide = os.path.join(results, "hide")
    os.mkdir(hide, 0o700)
    os.symlink(canonical, os.path.join(hide, name))
    os.chmod(hide, 0)


def deep_link(levels, segment, specs):
    """Nested directories built with relative steps (no path string is ever this long),
    then one symlink per KIND=NAME in the deepest one."""
    canonical = canonical_path()
    targets = {"canonical": canonical, "home": os.path.dirname(canonical), "root": "/"}
    start = os.getcwd()
    base = os.path.join(results_dir(), "deep")
    os.mkdir(base)
    os.chdir(base)
    length = len(base)
    for _ in range(levels):
        os.mkdir(segment)
        os.chdir(segment)
        length += 1 + len(segment)
    for spec in specs:
        kind, _sep, name = spec.partition("=")
        os.symlink(targets[kind], name)
    os.chdir(start)
    with open(os.path.join(results_dir(), "deep-path-length.txt"), "w") as handle:
        handle.write(str(length))


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
            elif action == "hard-link":
                os.link(os.readlink(os.path.join(home, "auth.json")),
                        os.path.join(os.path.dirname(home), "results", args[0]))
            elif action == "hidden-link":
                hidden_link(os.readlink(os.path.join(home, "auth.json")),
                            os.path.join(os.path.dirname(home), "results"), args[0])
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
                  "lease_fd_env": os.environ.get("GPTGREP_AUTH_LEASE_FD"), "auth_link": auth_link(),
                  "lease_descriptors": len(lease_descriptors(argv[3])) if len(argv) > 3 else None}
        with open(argv[2], "w") as handle:
            json.dump(report, handle)
        return 0
    if mode == "hold":
        seconds, ready = float(argv[2]), argv[3]
        if "GPTGREP_AUTH_LEASE_FD" in os.environ or (len(argv) > 4 and lease_descriptors(argv[4])):
            return 9
        open(ready, "w").close()
        time.sleep(seconds)
        return 0
    if mode == "unlock-lease":
        lease, seconds, ready, report = argv[2], float(argv[3]), argv[4], argv[5]
        inherited = lease_descriptors(lease)
        attempts = 0
        for fd in inherited:
            fcntl.flock(fd, fcntl.LOCK_UN)
            attempts += 1
        fresh = os.open(lease, os.O_RDONLY)
        fcntl.flock(fresh, fcntl.LOCK_UN)  # releases nothing: a lock belongs to its own open file
        attempts += 1
        with open(report, "w") as handle:
            json.dump({"inherited_lease_descriptors": len(inherited), "unlock_attempts": attempts}, handle)
        open(ready, "w").close()
        time.sleep(seconds)
        os.close(fresh)
        return 0
    if mode == "exit":
        return int(argv[2])
    if mode == "auth-fail":
        write_record(json.dumps({"schema": "gptgrep.codex-auth.auth-failure.v1", "class": argv[2]}))
        return 77
    if mode == "auth-record":
        write_record(argv[2])
        return int(argv[3])
    if mode == "auth-record-symlink":
        os.symlink(argv[2], os.environ["GPTGREP_AUTH_FAILURE_RECORD"])
        return int(argv[3])
    if mode == "link-credential":
        kind, name = argv[2], argv[3]
        target = os.path.join(results_dir(), name)
        if kind == "hard":
            os.link(canonical_path(), target)
        elif kind == "hard-outside":
            os.link(canonical_path(), name)
        elif kind == "sym-canonical":
            os.symlink(canonical_path(), target)
        elif kind == "sym-run-link":
            os.symlink(os.path.join("..", "codex-home", "auth.json"), target)
        elif kind == "sym-home":
            os.symlink(os.path.dirname(canonical_path()), target)
        else:
            return 2
        return 0
    if mode == "move-canonical-keep":
        canonical = canonical_path()
        os.link(canonical, os.path.join(results_dir(), argv[2]))
        replacement = canonical + ".replacement"
        fd = os.open(replacement, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, b'{"auth_mode": "chatgpt", "tokens": {"refresh_token": "FAKE-CANARY-REPLACEMENT"}}')
        os.close(fd)
        os.rename(replacement, canonical)
        return 0
    if mode == "symlink":
        os.symlink(argv[2], argv[3])
        return 0
    if mode == "deep-link":
        deep_link(int(argv[2]), argv[3], argv[4:])
        return 0
    if mode == "hidden-link":
        hidden_link(canonical_path(), results_dir(), argv[2])
        return 0
    if mode == "move-home":
        home = os.path.dirname(canonical_path())
        moved = os.path.join(results_dir(), argv[2])
        os.rename(home, moved)
        os.symlink(moved, home)
        os.link(os.path.join(moved, "auth.json"), os.path.join(moved, "auth2.json"))
        return 0
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
