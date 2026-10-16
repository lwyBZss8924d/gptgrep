"""Supervised consumer that starts one local container like a Harbor trial.

usage: container_consumer.py DOCKER_BIN IMAGE LABEL AUTH_FILE MODE SECONDS OUT READY
  MODE sleep      start the container detached (sleep SECONDS) and exit at once,
                  like a Harbor process whose trial container outlives it briefly
  MODE semantics  run the container attached; inside it, with `--network none`:
                  append to the read-only bind (expect EROFS), print READY, wait
                  for the host to rewrite the file in place, then connect to
                  127.0.0.1:0 (expect ECONNREFUSED); write the JSON result to OUT
Always `--pull never`, `--network none`, `--rm`, the given label, and the fake
credential bound read-only at /opt/full62-home/auth.json. Prints no file contents.
"""
import json
import subprocess
import sys

INSIDE = r"""
import errno, hashlib, json, os, socket, sys, time
path = "/opt/full62-home/auth.json"
def digest():
    # Fake credential only: compared inside the container, never printed.
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()
result = {"initial_size": os.path.getsize(path)}
initial = digest()
try:
    with open(path, "a") as handle:
        handle.write("x")
    result["write_errno"] = None
except OSError as exc:
    result["write_errno"] = errno.errorcode.get(exc.errno)
print("READY", flush=True)
deadline = time.time() + float(sys.argv[1])
while time.time() < deadline and digest() == initial:
    time.sleep(0.2)
result["host_rewrite_seen"] = digest() != initial
result["seen_after_seconds"] = round(float(sys.argv[1]) - (deadline - time.time()), 1)
s = socket.socket()
try:
    s.connect(("127.0.0.1", 0))
    result["port0"] = "connected"
except OSError as exc:
    result["port0"] = errno.errorcode.get(exc.errno)
finally:
    s.close()
print(json.dumps(result), flush=True)
"""


def main(argv):
    docker, image, label, auth_file, mode, seconds, out, ready = argv[1:9]
    base = [docker, "run", "--rm", "--pull", "never", "--network", "none", "--label", label,
            "--mount", "type=bind,source=%s,target=/opt/full62-home/auth.json,readonly" % auth_file]
    if mode == "sleep":
        proc = subprocess.run(base + ["-d", image, "sleep", seconds], capture_output=True, text=True, timeout=120)
        with open(out, "w") as handle:
            json.dump({"started": proc.returncode == 0}, handle)
        return proc.returncode
    proc = subprocess.Popen(base + [image, "python3", "-I", "-c", INSIDE, seconds], stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True)
    result = None
    for line in proc.stdout:
        if line.strip() == "READY":
            open(ready, "w").close()
        elif line.startswith("{"):
            result = json.loads(line)
    code = proc.wait(timeout=120)
    with open(out, "w") as handle:
        json.dump({"container_exit": code, "inside": result}, handle)
    return 0 if code == 0 and result else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
