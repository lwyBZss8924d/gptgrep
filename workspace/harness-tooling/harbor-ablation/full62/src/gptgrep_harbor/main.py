from __future__ import annotations

import argparse
import json
from pathlib import Path

from .adapter import Full62Adapter
from .aggregate import summarize
from .common import digest, parse, pinned, put, read, require, sha


def main():
    parser = argparse.ArgumentParser(description="Explicit admitted private full62 adapter")
    sub = parser.add_subparsers(dest="command", required=True)
    prepare = sub.add_parser(
        "prepare", help="Validate the admitted runtime, print its budget preflight, then write all 124 tasks",
        description="Validate the pinned manifest and its admitted runtime, print the budget preflight "
                    "(treatment tool cap against the uncapped bare arm, advisory warnings) as one JSON line "
                    "before any task is created, then materialize the complete 124-task cohort. "
                    "No Docker, Harbor or model calls.")
    prepare.add_argument("--manifest", required=True)
    prepare.add_argument("--manifest-sha256", required=True)
    prepare.add_argument("--output-dir", type=Path, required=True)
    prepare.add_argument("--limit", type=int)
    prepare.add_argument("--task-ids", nargs="+")
    prepare.add_argument("--overwrite", action="store_true")
    aggregate = sub.add_parser("aggregate", help="Summarize 124 pinned trial outputs in fixed 62-pair denominators")
    aggregate.add_argument("--inputs", required=True)
    aggregate.add_argument("--inputs-sha256", required=True)
    aggregate.add_argument("--output", required=True)
    local = sub.add_parser("build-local-images", help="Build pinned role images before Harbor; no model calls")
    local.add_argument("--export", required=True)
    local.add_argument("--export-sha256", required=True)
    local.add_argument("--base-builds", required=True)
    local.add_argument("--base-builds-sha256", required=True)
    local.add_argument("--dataset", required=True)
    local.add_argument("--output-dir", required=True)
    local.add_argument("--docker", required=True)
    local.add_argument("--timeout-seconds", type=int, required=True)
    args = parser.parse_args()
    if args.command == "prepare":
        adapter = Full62Adapter(args.manifest, args.manifest_sha256)
        # Validation output: warnings stay visible even if materialization stops.
        print(json.dumps({"preflight": adapter.preflight}, sort_keys=True), flush=True)
        adapter.run(args.output_dir, limit=args.limit, task_ids=args.task_ids, overwrite=args.overwrite)
    elif args.command == "build-local-images":
        from .local_images import build_dataset
        build_dataset(args.export, args.export_sha256, args.dataset, args.output_dir,
                      args.docker, args.timeout_seconds, args.base_builds, args.base_builds_sha256)
    else:
        data = read(str(Path(args.inputs).absolute()))
        require(sha(data) == digest(args.inputs_sha256), "aggregate_inputs_pin")
        inputs = parse(data)
        require(inputs["schema"] == "gptgrep.harbor.aggregate-inputs.v1", "aggregate_inputs")
        export = parse(pinned(inputs["export"]))
        runtime = parse(pinned(export["runtime"]))
        put(str(Path(args.output).absolute()), summarize(export, inputs["results"], runtime))


if __name__ == "__main__":
    main()
