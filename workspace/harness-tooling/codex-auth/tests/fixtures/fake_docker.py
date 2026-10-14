"""Fake Docker reads {"labels": {"KEY=VALUE": [ID, ...]}, "error": false}
from the state file named by `<this file>.json` ({"state": PATH}). A query
returns only ids under its exact label; no command contacts a daemon."""
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
    if not label.startswith("label="):
        return 2
    with open(STATE + ".calls", "a") as handle:
        handle.write(json.dumps({"filter": label, "docker_host": os.environ.get("DOCKER_HOST")}) + "\n")
    try:
        with open(STATE) as handle:
            state = json.load(handle)
    except (OSError, ValueError):
        return 1
    if state.get("error"):
        return 1
    ids = state.get("labels", {}).get(label[len("label="):], [])
    for container in ids:
        print(container)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
