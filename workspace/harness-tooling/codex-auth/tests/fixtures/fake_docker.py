"""Fake `docker` CLI: `ps -a -q --filter label=X` prints container ids listed in
the state file named by `<this file>.json` ({"state": PATH}); never contacts a daemon."""
import json
import os
import sys

SELF = os.path.realpath(sys.argv[0])
with open(SELF + ".json") as handle:
    STATE = json.load(handle)["state"]


def main(argv):
    if argv[1:4] != ["ps", "-a", "-q"] or "--filter" not in argv:
        return 2
    label = argv[argv.index("--filter") + 1]
    with open(STATE + ".calls", "a") as handle:
        handle.write(json.dumps({"filter": label, "docker_host": os.environ.get("DOCKER_HOST")}) + "\n")
    try:
        with open(STATE) as handle:
            ids = [line.strip() for line in handle if line.strip()]
    except FileNotFoundError:
        ids = []
    for container in ids:
        print(container)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
