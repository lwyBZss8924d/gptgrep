#!/usr/bin/env python3
"""Run GPTgrep retrieval checks against isolated, project-authored fixtures."""

from __future__ import annotations

import argparse
import datetime
from decimal import Decimal, InvalidOperation
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time
from typing import Any

REPO = Path(__file__).resolve().parents[1]


def percentile(values: list[float], fraction: float) -> float:
    if not values or not all(math.isfinite(value) and value >= 0 for value in values):
        raise ValueError("Latency samples must be nonempty, finite, and nonnegative")
    ordered = sorted(values)
    offset = (len(ordered) - 1) * fraction
    low, high = math.floor(offset), math.ceil(offset)
    return ordered[low] + (ordered[high] - ordered[low]) * (offset - low)


def decode_json(raw: str) -> dict[str, Any]:
    def invalid_constant(value: str) -> None:
        raise ValueError(f"Non-finite JSON number: {value}")

    result = json.loads(raw, parse_constant=invalid_constant)
    if not isinstance(result, dict):
        raise ValueError("CLI must return one JSON object")
    return result


def summarize_provider_cost(metric_samples: list[dict[str, Any]]) -> dict[str, Any]:
    """Sum explicit per-request provider costs only when every request is matched.

    An attempted call without a validated response, such as a failed submission
    repeated by the bounded transient retry, has unknown cost: the total stays
    unavailable rather than counting it as zero.
    """
    requests = missing = extra = 0
    costs: list[Decimal] = []
    for metrics in metric_samples:
        count = metrics.get("jev_requests", 0)
        count = count if type(count) is int and count > 0 else int(metrics.get("jev_used") is True)
        requests += count
        attempted = metrics.get("jev_calls_attempted")
        if type(attempted) is int and attempted > count:
            missing += attempted - count
        usage = metrics.get("jev_usage", [])
        usage = usage if isinstance(usage, list) else []
        extra += max(0, len(usage) - count)
        for index in range(count):
            entry = usage[index] if index < len(usage) else None
            cost = entry.get("cost") if isinstance(entry, dict) else None
            try:
                if isinstance(cost, bool) or not isinstance(cost, (int, float, str)):
                    raise ValueError("Cost is absent or not numeric")
                amount = Decimal(str(cost))
                if not amount.is_finite() or amount < 0 or not math.isfinite(float(amount)):
                    raise ValueError("Cost is not finite and nonnegative")
                costs.append(amount)
            except (InvalidOperation, ValueError, OverflowError):
                missing += 1
    complete = requests > 0 and missing == 0 and extra == 0
    total = sum(costs, Decimal(0)) if complete else None
    return {
        "measured_provider_cost_usd": float(total) if total is not None else None,
        "measured_provider_cost_decimal": str(total) if total is not None else None,
        "provider_cost_usd": float(total) if total is not None else None,
        "provider_cost_request_count": requests,
        "provider_cost_missing_requests": missing,
        "provider_cost_extra_receipts": extra,
        "cost_status": "all_observed_requests_have_explicit_provider_cost" if complete else (
            "no_provider_requests_observed" if requests == 0 and extra == 0 else "incomplete_provider_cost_receipts"
        ),
    }


def invoke(binary: Path, args: list[str], root: Path, timeout: float, allow_stale: bool = False) -> tuple[dict[str, Any], float, int]:
    started = time.perf_counter_ns()
    completed = subprocess.run(
        [str(binary), *args],
        cwd=root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=timeout,
        check=False,
    )
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if len(completed.stdout.encode("utf-8")) > 4_000_000:
        raise ValueError("CLI output exceeded the evaluation's 4 MB limit")
    if completed.returncode not in ((0, 1, 2) if allow_stale else (0, 1)):
        raise RuntimeError(f"CLI exit {completed.returncode}: {completed.stderr[:1000]}")
    envelope = decode_json(completed.stdout)
    if completed.returncode == 2 and not envelope.get("coverage", {}).get("stale_files"):
        raise RuntimeError("Exit 2 did not establish stale-source coverage")
    if completed.returncode == 1 and envelope.get("hits"):
        raise RuntimeError("Exit 1 with nonempty hits is not a successful retrieval")
    if args[0] == "index" and completed.returncode != 0:
        raise RuntimeError("Indexing failed")
    return envelope, elapsed_ms, len(completed.stdout.encode("utf-8"))


def relative_path(value: Any, root: Path) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError("Hit path must be a nonempty string")
    path = Path(value)
    resolved = (path if path.is_absolute() else root / path).resolve()
    try:
        return resolved.relative_to(root.resolve()).as_posix()
    except ValueError as exc:
        raise ValueError(f"Hit escapes corpus root: {value}") from exc


def validate_text_span(hit: dict[str, Any], data: bytes) -> None:
    """Check canonical UTF-8 text coordinates without normalizing CRLF bytes."""
    data.decode("utf-8")
    parts = data.split(b"\n")
    if data.endswith(b"\n"):
        parts.pop()
    bounds = []
    offset = 0
    for part in parts:
        bounds.append((offset, offset + len(part.rstrip(b"\r"))))
        offset += len(part) + 1
    start, end = hit.get("line_start"), hit.get("line_end")
    if type(start) is not int or type(end) is not int or not 1 <= start <= end <= len(bounds):
        raise ValueError(f"Invalid line bounds: {hit.get('path')}")
    excerpt = hit.get("text")
    low, high = bounds[start - 1][0], bounds[end - 1][1]
    if not isinstance(excerpt, str) or not excerpt:
        raise ValueError("Excerpt must be a nonempty string")
    if ("byte_start" in hit) != ("byte_end" in hit):
        raise ValueError("Partial byte range")
    if "byte_start" in hit:
        byte_start, byte_end = hit["byte_start"], hit["byte_end"]
        if type(byte_start) is not int or type(byte_end) is not int or not low <= byte_start < byte_end <= high:
            raise ValueError("Invalid byte range or line containment")
        if data[byte_start:byte_end].decode("utf-8") != excerpt:
            raise ValueError("Excerpt does not match exact byte range")
        if "column_start" in hit and (
            type(hit["column_start"]) is not int or hit["column_start"] != byte_start - low + 1
        ):
            raise ValueError("Invalid start column")
    elif excerpt not in data[low:high].decode("utf-8"):
        raise ValueError(f"Excerpt is not in the cited source span: {hit.get('path')}")
    match_line, match_column = hit.get("match_line"), hit.get("match_column")
    if match_column is not None:
        if type(match_line) is not int or not start <= match_line <= end:
            raise ValueError("Invalid match line")
        size = bounds[match_line - 1][1] - bounds[match_line - 1][0]
        if type(match_column) is not int or not 1 <= match_column <= size + 1:
            raise ValueError("Invalid match column")


def validate_hit(hit: dict[str, Any], root: Path) -> dict[str, Any]:
    result = dict(hit)
    result["path"] = relative_path(hit.get("path"), root)
    source = root / result["path"]
    data = source.read_bytes()
    expected_hash = hashlib.sha256(data).hexdigest()
    if hit.get("source_sha256") != expected_hash:
        raise ValueError(f"Source digest mismatch: {result['path']}")
    if hit.get("source_fresh") is not True:
        raise ValueError(f"Result is not confirmed fresh: {result['path']}")
    validate_text_span(hit, data)
    citation = hit.get("citation")
    if not isinstance(citation, (str, dict)) or not citation:
        raise ValueError("Missing citation")
    page_start, page_end = hit.get("page_start"), hit.get("page_end")
    if (page_start is None) != (page_end is None):
        raise ValueError("Partial page bounds")
    if page_start is not None and (
        type(page_start) is not int or type(page_end) is not int
        or not 1 <= page_start <= page_end
    ):
        raise ValueError("Invalid page bounds")
    score = hit.get("score")
    if isinstance(score, bool) or not isinstance(score, (int, float)) or not math.isfinite(score):
        raise ValueError("Score must be finite")
    return result


def matches(hit: dict[str, Any], expected: dict[str, Any]) -> bool:
    return all(
        value in hit.get("text", "") if field == "contains" else hit.get(field) == value
        for field, value in expected.items()
    )


def quality(hits: list[dict[str, Any]], relevant: list[dict[str, Any]], exact: bool) -> dict[str, Any]:
    # Low-confidence fallback hits did not pass the relevance floor; they are
    # validated as evidence but never scored as retrieved answers.
    hits = [hit for hit in hits if hit.get("below_threshold") is not True]
    if not relevant:
        return {
            "recall_at_k": None,
            "mrr_at_k": None,
            "no_answer_correct": not hits,
            "passed": not hits,
        }
    recovered = {i for i, item in enumerate(relevant) if any(matches(hit, item) for hit in hits)}
    first_rank = next(
        (rank for rank, hit in enumerate(hits, 1) if any(matches(hit, item) for item in relevant)),
        None,
    )
    exact_ok = len(hits) == len(relevant) and all(
        any(matches(hit, item) for item in relevant) for hit in hits
    )
    recall = len(recovered) / len(relevant)
    return {
        "recall_at_k": recall,
        "mrr_at_k": 1 / first_rank if first_rank else 0.0,
        "no_answer_correct": None,
        "passed": recall == 1.0 and (not exact or exact_ok),
    }


def checked_hits(envelope: dict[str, Any], root: Path, limit: int) -> list[dict[str, Any]]:
    if not envelope.get("schema_version"):
        raise ValueError("Missing response schema_version")
    if envelope.get("index_used") is not True:
        raise ValueError("Search did not establish use of the built index")
    hits = envelope.get("hits")
    if not isinstance(hits, list) or len(hits) > limit:
        raise ValueError("Invalid or oversized hits array")
    if not all(isinstance(hit, dict) for hit in hits):
        raise ValueError("Every hit must be an object")
    return [validate_hit(hit, root) for hit in hits]


def stale_probe(binary: Path, root: Path, timeout: float, query: str) -> dict[str, Any]:
    envelope, elapsed, _ = invoke(
        binary, ["search", query, str(root), "--mode", "regex", "--json", "--limit", "5"], root, timeout,
        allow_stale=True,
    )
    hits = envelope.get("hits")
    if not isinstance(hits, list):
        error = json.dumps(envelope.get("error", ""), ensure_ascii=False).lower()
        if not any(word in error for word in ("stale", "changed", "deleted", "missing")):
            raise ValueError("Freshness probe returned an unrelated error")
        return {"passed": True, "behavior": "explicit_stale_error", "latency_ms": elapsed}
    stale_count = 0
    for hit in hits:
        # A still-fresh hit must resolve to current bytes and truthful evidence.
        if hit.get("source_fresh") is True:
            validate_hit(hit, root)
            raise ValueError("Obsolete unique marker was returned as fresh")
        if hit.get("source_fresh") is not False:
            raise ValueError("Stale hit has no explicit freshness status")
        stale_count += 1
    return {
        "passed": True,
        "behavior": "stale_marked" if stale_count else "obsolete_evidence_not_returned",
        "latency_ms": elapsed,
    }


def lifecycle(binary: Path, root: Path, timeout: float) -> dict[str, Any]:
    recovery = root / "ops/recovery.md"
    before = recovery.stat()
    original = recovery.read_bytes()
    changed = original.replace(b"SNAP-42", b"SNAP-99")
    if changed == original or len(changed) != len(original):
        raise ValueError("Freshness fixture must change bytes without changing file size")
    recovery.write_bytes(changed)
    os.utime(recovery, ns=(before.st_atime_ns, before.st_mtime_ns))
    replacement = stale_probe(binary, root, timeout, "SNAP-42")
    invoke(binary, ["index", str(root), "--json"], root, timeout)
    updated, _, _ = invoke(
        binary, ["search", "SNAP-99", str(root), "--mode", "regex", "--json", "--limit", "5"], root, timeout
    )
    updated_hits = checked_hits(updated, root, 5)
    replacement_result = quality(
        updated_hits, [{"path": "ops/recovery.md", "line_start": 6, "line_end": 6}], True
    )
    # Isolate deletion after the changed-file index is fresh again.
    (root / "architecture/storage.mdx").unlink()
    deletion = stale_probe(binary, root, timeout, "RETENTION_WINDOW_37")
    invoke(binary, ["index", str(root), "--json"], root, timeout)
    deleted, _, _ = invoke(
        binary, ["search", "RETENTION_WINDOW_37", str(root), "--mode", "regex", "--json", "--limit", "5"],
        root, timeout,
    )
    deletion_result = quality(checked_hits(deleted, root, 5), [], True)
    return {
        "same_size_same_mtime_replacement": replacement,
        "deleted_source": deletion,
        "reindexed_replacement": replacement_result,
        "reindexed_deletion": deletion_result,
        "passed": replacement_result["passed"] and deletion_result["passed"],
    }


def evaluate(args: argparse.Namespace) -> dict[str, Any]:
    binary = args.binary.expanduser().resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"Build GPTgrep first; binary not found: {binary}")
    qrel_path = args.qrels.expanduser().resolve()
    qrels = decode_json(qrel_path.read_text(encoding="utf-8"))
    corpus = (qrel_path.parent / qrels["corpus"]).resolve()
    rows: list[dict[str, Any]] = []
    all_latencies: list[float] = []
    actual_jev_requests = 0
    with tempfile.TemporaryDirectory(prefix="gptgrep-eval-") as directory:
        root = Path(directory) / "corpus"
        shutil.copytree(corpus, root)
        corpus_manifest = [
            {"path": file.relative_to(root).as_posix(), "sha256": hashlib.sha256(file.read_bytes()).hexdigest(), "bytes": file.stat().st_size}
            for file in sorted(root.rglob("*")) if file.is_file()
        ]
        corpus_bytes = sum(item["bytes"] for item in corpus_manifest)
        _, index_ms, _ = invoke(binary, ["index", str(root), "--json"], root, args.timeout)
        cases = [(item, item["mode"], args.repetitions) for item in qrels["queries"]]
        if args.jev:
            cases += [(item, "hybrid", 1) for item in qrels["queries"] if item["mode"] == "lexical"]
        for item, mode, repetitions in cases:
            samples: list[float] = []
            qualities: list[dict[str, Any]] = []
            returned_bytes: list[int] = []
            metric_samples: list[dict[str, Any]] = []
            hit_count = 0
            for _ in range(repetitions):
                env, elapsed, wire_bytes = invoke(
                    binary,
                    ["search", item["query"], str(root), "--mode", mode, "--json", "--limit", str(args.limit)],
                    root, args.timeout,
                )
                hits = checked_hits(env, root, args.limit)
                qualities.append(quality(hits, item["relevant"], item.get("exact", False)))
                samples.append(elapsed)
                returned_bytes.append(wire_bytes)
                hit_count += len(hits)
                metrics = env.get("metrics", {})
                metric_samples.append(metrics)
                requests = metrics.get("jev_requests", 0)
                if type(requests) is int and requests > 0:
                    actual_jev_requests += requests
                elif metrics.get("jev_used") is True:
                    actual_jev_requests += 1
            all_latencies.extend(samples)
            rows.append({
                "id": item["id"],
                "mode": mode,
                "samples": repetitions,
                "recall_at_k": statistics.mean(q["recall_at_k"] for q in qualities) if item["relevant"] else None,
                "mrr_at_k": statistics.mean(q["mrr_at_k"] for q in qualities) if item["relevant"] else None,
                "no_answer_correct": all(q["no_answer_correct"] for q in qualities) if not item["relevant"] else None,
                "passed": all(q["passed"] for q in qualities),
                "citation_checks": hit_count,
                "latency_ms": {"p50": percentile(samples, 0.5), "p95": percentile(samples, 0.95)},
                "mean_response_bytes": statistics.mean(returned_bytes),
                "engine_metrics": metric_samples,
            })
        freshness = lifecycle(binary, root, args.timeout)
    positive = [row for row in rows if row["recall_at_k"] is not None]
    by_mode = {}
    for mode in sorted({row["mode"] for row in rows}):
        selected = [row for row in rows if row["mode"] == mode]
        scored = [row for row in selected if row["recall_at_k"] is not None]
        by_mode[mode] = {
            "cases": len(selected),
            "macro_recall_at_k": statistics.mean(row["recall_at_k"] for row in scored) if scored else None,
            "macro_mrr_at_k": statistics.mean(row["mrr_at_k"] for row in scored) if scored else None,
            "passed": all(row["passed"] for row in selected),
        }
    jev_observed = actual_jev_requests > 0
    jev_requirement_satisfied = not args.jev or jev_observed
    return {
        "schema_version": "gptgrep.eval.result.v1",
        "observed_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "native_session_id": os.environ.get("CODEX_THREAD_ID"),
        "passed": all(row["passed"] for row in rows) and freshness["passed"] and jev_requirement_satisfied,
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "qrels_sha256": hashlib.sha256(qrel_path.read_bytes()).hexdigest(),
        "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "corpus_manifest": corpus_manifest,
        "corpus_bytes": corpus_bytes,
        "limit": args.limit,
        "query_cases": len(rows),
        "search_samples": len(all_latencies),
        "index_latency_ms": index_ms,
        "macro_recall_at_k": statistics.mean(row["recall_at_k"] for row in positive),
        "macro_mrr_at_k": statistics.mean(row["mrr_at_k"] for row in positive),
        "summary_by_mode": by_mode,
        "latency_ms": {"p50": percentile(all_latencies, 0.5), "p95": percentile(all_latencies, 0.95)},
        "jev_requested": args.jev,
        "observed_jev_requests": actual_jev_requests,
        "jev_observed": jev_observed,
        "jev_requirement_satisfied": jev_requirement_satisfied,
        **summarize_provider_cost([metrics for row in rows for metrics in row["engine_metrics"]]),
        "citation_checks": sum(row["citation_checks"] for row in rows),
        "freshness": freshness,
        "results": rows,
        "limitations": [
            "Small synthetic invariant suite; not a representative retrieval benchmark",
            "Reported percentiles describe these subprocess samples, not production tail guarantees",
            "Plain-text citations only; PDF/page/parser mappings need separate fixtures",
            "No upstream PageIndex run or external semantic judge is included",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/gptgrep")
    parser.add_argument("--qrels", type=Path, default=REPO / "evals/qrels.json")
    parser.add_argument("--limit", type=int, default=5)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--jev", action="store_true", help="Explicitly allow live hybrid requests; may incur provider charges")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.limit < 1 or args.repetitions < 1 or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("limit, repetitions and timeout must be positive")
    try:
        report = evaluate(args)
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as error:
        report = {"schema_version": "gptgrep.eval.result.v1", "passed": False, "error": str(error)}
    payload = json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(payload, encoding="utf-8")
    print(payload, end="")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
