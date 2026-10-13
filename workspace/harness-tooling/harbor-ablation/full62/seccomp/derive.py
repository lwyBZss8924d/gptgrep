"""Derive the full62 reader seccomp profile from Docker 29.4.0's default profile.

The owner-approved change (2026-10-13) removes unshare, clone, clone3, setns,
mount, umount2 and pivot_root from every upstream rule and appends one
unconditional SCMP_ACT_ALLOW rule for exactly those seven calls, which Codex's
bundled bubblewrap needs to create its user, mount and PID namespaces. Every
other rule, condition and top-level field is copied unchanged, in order.
Upstream has no pivot_root rule, so that call was denied by the default action.

Standard library only. The derivation accepts only the pinned upstream bytes
and serializes deterministically; it uses no network, Docker or file discovery.

    python3 -B seccomp/derive.py --check
    python3 -B seccomp/derive.py --upstream /fresh/default.json --output /new/profile.json
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
UPSTREAM = HERE / "upstream" / "default.json"
DERIVED = HERE / "gptgrep-reader-seccomp.json"
# moby/moby docker-v29.4.0 (daa0cb7f23594cdbcce5002e370027d3fd36ffd7):
# vendor/github.com/moby/profiles/seccomp/default.json (module v0.1.0).
UPSTREAM_BYTES = 13063
UPSTREAM_SHA256 = "01536f1d1df938ae611eba20d6349e0de7a99b6ecdee1549427a0b01b8301e28"
SYSCALLS = ("unshare", "clone", "clone3", "setns", "mount", "umount2", "pivot_root")
COMMENT = ("gptgrep full62 reader: unprivileged user/mount/pid namespaces for Codex bubblewrap "
           "(owner-approved 2026-10-13)")


def _unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate_json_key")
        result[key] = value
    return result


def derive(upstream: bytes) -> bytes:
    """Return the reader profile's exact bytes for the pinned upstream profile bytes."""
    if len(upstream) != UPSTREAM_BYTES or hashlib.sha256(upstream).hexdigest() != UPSTREAM_SHA256:
        raise ValueError("upstream_profile_pin")
    profile = json.loads(upstream, object_pairs_hook=_unique)
    rules = []
    for rule in profile["syscalls"]:
        names = [name for name in rule["names"] if name not in SYSCALLS]
        # A rule left without names (the flag-filtered clone and the clone3
        # ENOSYS rules) is dropped; any other rule keeps its conditions.
        if names:
            rules.append({**rule, "names": names})
    rules.append({"names": list(SYSCALLS), "action": "SCMP_ACT_ALLOW", "comment": COMMENT})
    profile["syscalls"] = rules
    return (json.dumps(profile, indent=1, ensure_ascii=True) + "\n").encode("ascii")


def summary(data: bytes, **extra) -> str:
    return json.dumps({"derived_bytes": len(data), "derived_sha256": hashlib.sha256(data).hexdigest(),
                       **extra}, sort_keys=True)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Derive the full62 reader seccomp profile from the "
                                     "pinned Docker 29.4.0 default profile")
    parser.add_argument("--upstream", type=Path, default=UPSTREAM,
                        help="pinned upstream default.json (default: the vendored copy)")
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--check", action="store_true",
                        help="require byte equality with the committed gptgrep-reader-seccomp.json")
    action.add_argument("--output", type=Path, help="write the derived profile to a new file; never overwrites")
    args = parser.parse_args(argv)
    data = derive(args.upstream.read_bytes())
    if args.check:
        matches = DERIVED.read_bytes() == data
        print(summary(data, matches_committed=matches))
        return 0 if matches else 1
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
    with os.fdopen(fd, "wb") as target:
        target.write(data)
    print(summary(data, output=str(args.output)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
