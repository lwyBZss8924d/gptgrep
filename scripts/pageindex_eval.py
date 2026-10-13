#!/usr/bin/env python3
"""Measure GPTgrep on a pinned local PageIndex benchmark subset; default is offline."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import datetime
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

import eval as oracle

REPO = Path(__file__).resolve().parents[1]


def sha256(file: Path) -> str:
    return hashlib.sha256(file.read_bytes()).hexdigest()


def materialize_subset(questions: list[dict[str, Any]], subset: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Gold text is loaded from the verified upstream file, never public metadata."""
    result = []
    for metadata in subset:
        allowed = {"row_index_zero_based", "row_sha256", "question_sha256", "doc_id",
                   "document_sha256", "document_pages_metadata"}
        if set(metadata) - allowed:
            raise ValueError("Reference metadata must contain provenance only, not task text or gold labels")
        index = metadata["row_index_zero_based"]
        if type(index) is not int or not 0 <= index < len(questions):
            raise ValueError("Invalid source question index")
        row = questions[index]
        encoded = json.dumps(row, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
        if hashlib.sha256(encoded).hexdigest() != metadata["row_sha256"]:
            raise ValueError(f"Question row digest mismatch at row {index}")
        if hashlib.sha256(row["question"].encode()).hexdigest() != metadata["question_sha256"]:
            raise ValueError(f"Question text digest mismatch at row {index}")
        if metadata["doc_id"] != row["doc_id"]:
            raise ValueError(f"Question metadata mismatch at row {index}")
        result.append({**row, **metadata})
    return result


def run_cli(binary: Path, args: list[str], cwd: Path, timeout: float) -> tuple[dict[str, Any], float]:
    started = time.perf_counter_ns()
    process = subprocess.run(
        [str(binary), *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8",
        timeout=timeout, check=False,
    )
    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    if len(process.stdout.encode()) > 16_000_000:
        raise ValueError("CLI output exceeded the 16 MB evaluation bound")
    envelope = oracle.decode_json(process.stdout)
    if process.returncode not in (0, 1) or (process.returncode == 1 and envelope.get("hits") != []):
        raise RuntimeError(f"CLI exit {process.returncode}: {str(envelope.get('error', 'unspecified error'))[:800]}")
    return envelope, elapsed


def verified_reference(reference: Path, manifest: dict[str, Any]) -> list[dict[str, Any]]:
    revision = subprocess.run(
        ["git", "-C", str(reference), "rev-parse", "HEAD"], capture_output=True, text=True, check=True
    ).stdout.strip()
    if revision != manifest["git_revision"]:
        raise ValueError("Reference checkout revision differs from the pinned manifest")
    for filename, field in (
        ("questions.json", "questions_sha256"),
        ("documents.json", "documents_metadata_sha256"),
        ("results.json", "results_sha256"),
    ):
        if sha256(reference / filename) != manifest[field]:
            raise ValueError(f"Pinned metadata changed: {filename}")
    questions = json.loads((reference / "questions.json").read_text())
    subset = materialize_subset(questions, manifest["smoke_subset"])
    for row in subset:
        index = row["row_index_zero_based"]
        filename = oracle.relative_path(row["doc_id"], reference / "documents")
        if filename != row["doc_id"]:
            raise ValueError("Reference document name is not canonical")
        if sha256(reference / "documents" / filename) != row["document_sha256"]:
            raise ValueError(f"Source PDF digest mismatch: {filename}")
        pages = json.loads(row["evidence_pages"])
        if not pages or any(type(page) is not int or not 1 <= page <= row["document_pages_metadata"] for page in pages):
            raise ValueError(f"Invalid physical evidence pages at row {index}")
    if len(subset) != manifest["smoke_subset_question_count"]:
        raise ValueError("Subset count does not match manifest")
    return subset


@contextmanager
def work_directory(requested: Path | None):
    if requested is None:
        with tempfile.TemporaryDirectory(prefix="gptgrep-pageindex-eval-") as temporary:
            yield Path(temporary)
        return
    destination = requested.expanduser().resolve()
    if destination.is_relative_to(REPO):
        ignored = subprocess.run(
            ["git", "-C", str(REPO), "check-ignore", "-q", str(destination / "probe.pdf")],
            check=False,
        )
        if ignored.returncode != 0:
            raise ValueError("External PDFs may only persist outside the repository or in a gitignored private directory")
    if destination.exists():
        raise ValueError("Persistent corpus destination must not already exist")
    destination.mkdir(mode=0o700, parents=True)
    yield destination


def page_quality(hits: list[dict[str, Any]], doc_id: str, gold_pages: set[int]) -> dict[str, Any]:
    # Low-confidence fallback hits did not pass the relevance floor. They are
    # still validated as evidence, but never scored as retrieved pages, the
    # same rule as scripts/eval.py quality().
    hits = [hit for hit in hits if hit.get("below_threshold") is not True]
    recovered: set[int] = set()
    first_doc, first_page = None, None
    for rank, hit in enumerate(hits, 1):
        if hit["path"] != doc_id:
            continue
        if first_doc is None:
            first_doc = rank
        overlap = gold_pages.intersection(range(hit["page_start"], hit["page_end"] + 1))
        recovered.update(overlap)
        if overlap and first_page is None:
            first_page = rank
    return {
        "document_recall_at_k": float(first_doc is not None),
        "document_mrr_at_k": 1 / first_doc if first_doc else 0.0,
        "physical_page_recall_at_k": len(recovered) / len(gold_pages),
        "physical_page_mrr_at_k": 1 / first_page if first_page else 0.0,
        "recovered_gold_pages": sorted(recovered),
    }


def validate_pdf_hit(hit: dict[str, Any], root: Path, parsed: dict[str, dict[str, Any]]) -> dict[str, Any]:
    normalized = {**hit, "path": oracle.relative_path(hit.get("path"), root)}
    path = normalized["path"]
    source = root / path
    if hit.get("source_fresh") is not True or hit.get("source_sha256") != sha256(source):
        raise ValueError("PDF result is stale or source digest differs")
    if hit.get("coordinate_system") != "extracted_lines_and_source_pages":
        raise ValueError("PDF hit does not declare physical-page coordinates")
    canonical = parsed[path]
    oracle.validate_text_span(hit, canonical["text"].encode("utf-8"))
    start, end = hit.get("page_start"), hit.get("page_end")
    if type(start) is not int or type(end) is not int or not 1 <= start <= end <= len(canonical["pages"]):
        raise ValueError("Invalid PDF physical-page range")
    actual_pages = [
        page["number"] for page in canonical["pages"]
        if page["line_start"] <= hit["line_end"] and page["line_end"] >= hit["line_start"]
    ]
    if not actual_pages or start != min(actual_pages) or end != max(actual_pages):
        raise ValueError("PDF page range does not match the extracted line mapping")
    if not isinstance(hit.get("citation"), str) or not hit["citation"]:
        raise ValueError("PDF hit has no citation")
    if not isinstance(hit.get("score"), (int, float)) or isinstance(hit["score"], bool) or not math.isfinite(hit["score"]):
        raise ValueError("PDF hit score is not finite")
    return normalized


def evaluate(args: argparse.Namespace) -> dict[str, Any]:
    binary = args.binary.expanduser().resolve()
    reference = args.reference_root.expanduser().resolve()
    manifest_path = args.manifest.expanduser().resolve()
    manifest = oracle.decode_json(manifest_path.read_text())
    subset = verified_reference(reference, manifest)
    binary_digest = sha256(binary)
    unique = {row["doc_id"]: row for row in subset}
    scopes = ["known-doc", "broad"] if args.scope == "both" else [args.scope]
    parsed: dict[str, dict[str, Any]] = {}
    extraction: list[dict[str, Any]] = []
    index_receipts: list[dict[str, Any]] = []
    results: list[dict[str, Any]] = []
    latencies: list[float] = []
    actual_jev_requests = 0
    with work_directory(args.corpus_dir) as work:
        # Parse for coverage and citation validation. Text stays in memory and is not emitted.
        for doc_id, row in unique.items():
            source = reference / "documents" / doc_id
            record: dict[str, Any] = {
                "doc_id": doc_id, "source_sha256": row["document_sha256"],
                "expected_physical_pages": row["document_pages_metadata"],
            }
            try:
                document, elapsed = run_cli(binary, ["parse", str(source), "--json"], work, args.timeout)
                pages = document["pages"]
                if len(pages) != row["document_pages_metadata"] or [p["number"] for p in pages] != list(range(1, len(pages) + 1)):
                    raise ValueError("Extracted page coverage differs from source metadata")
                if not document.get("text", "").strip():
                    raise ValueError("No searchable canonical text")
                parsed[doc_id] = document
                record.update(
                    status="parsed", physical_pages=len(pages), parser=document["parser"],
                    canonical_text_sha256=hashlib.sha256(document["text"].encode()).hexdigest(),
                    extracted_bytes=len(document["text"].encode()), validation_parse_ms=elapsed,
                    warnings=document.get("warnings", []),
                )
            except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as error:
                record.update(status="parse_failed", error=str(error))
            extraction.append(record)
        for scope in scopes:
            groups = {doc_id: [doc_id] for doc_id in unique} if scope == "known-doc" else {"all": list(unique)}
            roots: dict[str, Path] = {}
            available: set[str] = set()
            for group, filenames in groups.items():
                eligible = [name for name in filenames if name in parsed]
                if not eligible:
                    continue
                group_id = hashlib.sha256(group.encode()).hexdigest()[:12]
                root = work / scope / group_id
                root.mkdir(parents=True)
                for name in eligible:
                    shutil.copyfile(reference / "documents" / name, root / name)
                    if sha256(root / name) != unique[name]["document_sha256"]:
                        raise ValueError("Source bytes changed while copying the local corpus")
                roots[group] = root
                receipt: dict[str, Any] = {"scope": scope, "group": group, "source_documents": len(eligible)}
                try:
                    indexed, elapsed = run_cli(binary, ["index", str(root), "--json"], root, args.timeout)
                    if indexed.get("indexed_files") != len(eligible):
                        raise ValueError("Index did not retain every eligible source document")
                    receipt.update(status="indexed", index_ms=elapsed, engine=indexed)
                    # Root is ephemeral; no private absolute path is required in the durable receipt.
                    receipt["engine"].pop("root", None)
                    available.add(group)
                except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as error:
                    receipt.update(status="index_failed", error=str(error))
                index_receipts.append(receipt)
            for row in subset:
                group = row["doc_id"] if scope == "known-doc" else "all"
                gold_pages = set(json.loads(row["evidence_pages"]))
                result: dict[str, Any] = {
                    "source_question_row": row["row_index_zero_based"], "doc_id": row["doc_id"],
                    "question_sha256": hashlib.sha256(row["question"].encode()).hexdigest(),
                    "scope": scope, "mode": args.mode, "gold_physical_pages": sorted(gold_pages),
                    **page_quality([], row["doc_id"], gold_pages),
                }
                if row["doc_id"] not in parsed or group not in available:
                    result["status"] = "not_searched_due_to_ingestion_failure"
                    results.append(result)
                    continue
                root = roots[group]
                try:
                    response, elapsed = run_cli(
                        binary, ["search", row["question"], str(root), "--mode", args.mode, "--limit", str(args.limit), "--json"],
                        root, args.timeout,
                    )
                    if not response.get("schema_version") or response.get("index_used") is not True:
                        raise ValueError("Search did not establish a versioned indexed result")
                    raw_hits = response["hits"]
                    if not isinstance(raw_hits, list) or len(raw_hits) > args.limit:
                        raise ValueError("Invalid or oversized hits")
                    hits = [validate_pdf_hit(hit, root, parsed) for hit in raw_hits]
                    result.update(page_quality(hits, row["doc_id"], gold_pages))
                    metrics = response.get("metrics", {})
                    requests = metrics.get("jev_requests", 0)
                    if type(requests) is int and requests > 0:
                        actual_jev_requests += requests
                    result.update(
                        status="measured", latency_ms=elapsed, engine_metrics=metrics,
                        index_used=response["index_used"], generation=response.get("generation"),
                        coverage=response.get("coverage"), warnings=response.get("warnings", []),
                        citation_checks=len(hits), returned_text_bytes=sum(len(hit["text"].encode()) for hit in hits),
                        below_threshold_hits=sum(hit.get("below_threshold") is True for hit in hits),
                        hits=[{
                            key: hit.get(key) for key in (
                                "path", "node_id", "line_start", "line_end", "page_start", "page_end",
                                "byte_start", "byte_end", "source_sha256", "source_fresh", "text_truncated",
                                "below_threshold",
                            )
                        } for hit in hits],
                    )
                    latencies.append(elapsed)
                except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as error:
                    result.update(status="search_or_citation_failed", error=str(error))
                results.append(result)
        persistence = str(work) if args.corpus_dir is not None else None
    if sha256(binary) != binary_digest:
        raise ValueError("Binary changed during the evaluation")
    by_scope = {}
    for scope in scopes:
        selected = [row for row in results if row["scope"] == scope]
        by_scope[scope] = {
            "query_denominator": len(selected),
            "measured_queries": sum(row["status"] == "measured" for row in selected),
            **{
                key: statistics.mean(row[key] for row in selected) for key in (
                    "document_recall_at_k", "document_mrr_at_k", "physical_page_recall_at_k", "physical_page_mrr_at_k"
                )
            },
        }
    jev_observed = actual_jev_requests > 0
    complete = all(row["status"] == "measured" for row in results)
    return {
        "schema_version": "gptgrep.eval.pageindex-reference.v1",
        "status": "measured" if complete and (args.mode != "hybrid" or jev_observed) else "incomplete",
        "observed_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "native_session_id": os.environ.get("CODEX_THREAD_ID"),
        "binary_sha256": binary_digest, "harness_sha256": sha256(Path(__file__)),
        "reference_manifest_sha256": sha256(manifest_path), "reference_revision": manifest["git_revision"],
        "mode": args.mode, "limit": args.limit, "summary_by_scope": by_scope,
        "input_documents": len(unique), "parsed_documents": len(parsed),
        "extraction_coverage": len(parsed) / len(unique), "extraction": extraction,
        "indexing": index_receipts, "results": results,
        "latency_ms": {
            "samples": len(latencies),
            "p50": oracle.percentile(latencies, 0.5) if latencies else None,
            "p95": oracle.percentile(latencies, 0.95) if latencies else None,
        },
        "jev_observed": jev_observed, "observed_jev_requests": actual_jev_requests,
        **oracle.summarize_provider_cost([row["engine_metrics"] for row in results if "engine_metrics" in row]),
        "upstream_pageindex_sdk_executed": False, "answer_judge_executed": False,
        "answer_support_verified": False, "persistent_private_corpus": persistence,
        "limitations": [
            "GPTgrep measured on an upstream corpus subset; no PageIndex SDK baseline is executed",
            "Expected physical-page labels are retained upstream labels, not newly adjudicated ground truth",
            "Known-doc scope supplies the source document; broad scope retains original potentially ambiguous questions",
            "Page recall is not answer correctness or semantic support verification",
            "Parse validation is extra evaluation work; its time is separate from index and search latency",
            "Failures remain in the recall denominator; source text and answers are not emitted in this report",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference-root", required=True, type=Path)
    parser.add_argument("--manifest", type=Path, default=REPO / "evals/pageindex-reference.json")
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/gptgrep")
    parser.add_argument("--scope", choices=["known-doc", "broad", "both"], default="known-doc")
    parser.add_argument("--mode", choices=["lexical", "hybrid"], default="lexical",
                        help="hybrid explicitly sends bounded candidate text to the configured provider and may incur charges")
    parser.add_argument("--limit", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--corpus-dir", type=Path, help="Optional new private directory retaining copied PDFs and indexes")
    parser.add_argument("--output", type=Path, help="Optional metadata-only JSON receipt")
    args = parser.parse_args()
    if not 1 <= args.limit <= 1000 or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("limit must be 1..1000 and timeout must be positive")
    try:
        report = evaluate(args)
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
        report = {"schema_version": "gptgrep.eval.pageindex-reference.v1", "status": "failed", "error": str(error)}
    payload = json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(payload, encoding="utf-8")
    print(payload, end="")
    return 0 if report["status"] == "measured" else 1


if __name__ == "__main__":
    raise SystemExit(main())
