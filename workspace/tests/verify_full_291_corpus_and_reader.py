#!/usr/bin/env python3
"""Full 291-Document / 82,121-Page Corpus & Microsoft Native MarkdownReader Verifier.

Verifies across all 291 active XL-DocBench documents (82,121 expected pages):
1. Zero duplicate folders or files (' (1)' etc.) in OCR_ROOT.
2. Every page 0..expected_pages-1 has:
   - <doc_id>_page<i>/vlm/<doc_id>_page<i>.md (<= 5 MB)
   - <doc_id>_page<i>/vlm/<doc_id>_page<i>_middle.json
   - <doc_id>_page<i>/vlm/<doc_id>_page<i>_model.json
   - <doc_id>_page<i>/vlm/<doc_id>_page<i>_structured_content.json
3. Every MinerU asset referenced in .md, _middle.json, or _structured_content.json
   physically exists in <doc_id>_page<i>/vlm/images/, is > 0 bytes and <= 20 MB.
4. Microsoft native MarkdownReader(OCR_ROOT, doc_id).read_pages(1..expected_pages)
   succeeds with missing == [] across all 291 documents (82,121 pages).
"""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])


def _resolve_volume_path() -> Path:
    env_vol = os.environ.get("GPTGREP_STORAGE_VOLUME")
    if env_vol:
        return Path(env_vol)
    if len(WORKTREE_ROOT.parts) >= 3 and WORKTREE_ROOT.parts[1] == "Volumes":
        return Path(*WORKTREE_ROOT.parts[:3])
    return WORKTREE_ROOT


OCR_ROOT = Path(
    os.environ.get("XL_DOCBENCH_OCR_ROOT")
    or (_resolve_volume_path() / "g-drive" / "XL-DocBench-OCR-20261018")
)
INVENTORY_PATH = (
    WORKTREE_ROOT
    / "workspace/handoff/google-agy-xl-docbench-ocr-20261018-r1/codex-intake-20261021-r1/local-inventory.json"
)
DOCATLAS_PATH = Path(
    os.environ.get("DOCATLAS_HARNESS_ROOT")
    or (Path.home() / "dev-space" / "microsoft" / "DocAtlas-Harness")
)
if str(DOCATLAS_PATH) not in sys.path:
    sys.path.insert(0, str(DOCATLAS_PATH))

from docatlas.skills._common.markdown_reader import MarkdownReader

MAX_MD_BYTES = 5 * 1024 * 1024
MAX_IMG_BYTES = 20 * 1024 * 1024
ASSET_RE = re.compile(r"(page_0_[a-zA-Z0-9_]+\.(?:jpg|jpeg|png))")


def verify_document(doc_entry: dict) -> dict:
    doc_id = doc_entry["document_id"]
    exp_pages = doc_entry["page_count"]
    doc_dir = OCR_ROOT / doc_id
    errors = []

    if not doc_dir.is_dir():
        return {"doc_id": doc_id, "ok": False, "errors": [f"Missing doc_dir: {doc_dir}"]}

    dups = [str(p.relative_to(OCR_ROOT)) for p in doc_dir.rglob("* (*")]
    if dups:
        errors.append(f"Duplicate paths found: {dups[:5]}")

    md_files = 0
    md_bytes = 0
    zero_md_pages = []
    img_files = 0
    img_bytes = 0

    for p in range(exp_pages):
        stem = f"{doc_id}_page{p}"
        vlm = doc_dir / stem / "vlm"
        md_path = vlm / f"{stem}.md"
        if not md_path.is_file():
            errors.append(f"Page {p}: missing {stem}.md")
        else:
            sz = md_path.stat().st_size
            md_files += 1
            md_bytes += sz
            if sz == 0:
                zero_md_pages.append(p)
            if sz > MAX_MD_BYTES:
                errors.append(f"Page {p}: md exceeds 5MB ({sz})")

        for suf in ["_middle.json", "_model.json", "_structured_content.json"]:
            jp = vlm / f"{stem}{suf}"
            if not jp.is_file():
                errors.append(f"Page {p}: missing {stem}{suf}")

        idir = vlm / "images"
        d_set = set()
        if idir.is_dir():
            for x in idir.iterdir():
                if x.is_file() and x.suffix.lower() in (".jpg", ".jpeg", ".png"):
                    isz = x.stat().st_size
                    if isz <= 0 or isz > MAX_IMG_BYTES:
                        errors.append(f"Page {p}: invalid image size {x.name} ({isz})")
                    d_set.add(x.name)
                    img_files += 1
                    img_bytes += isz

        j_set = set()
        for suf in [".md", "_middle.json", "_structured_content.json"]:
            fp = vlm / f"{stem}{suf}"
            if fp.is_file():
                j_set.update(ASSET_RE.findall(fp.read_text(encoding="utf-8", errors="replace")))
        missing_imgs = sorted(j_set - d_set)
        if missing_imgs:
            errors.append(f"Page {p}: missing referenced images {missing_imgs}")

    # Native Microsoft MarkdownReader verification across ALL 1..exp_pages
    reader = MarkdownReader(str(OCR_ROOT), doc_id)
    if not reader.available:
        errors.append("MarkdownReader.available is False")
    else:
        found, missing = reader.read_pages(list(range(1, exp_pages + 1)))
        if missing:
            errors.append(f"MarkdownReader missing {len(missing)} pages: {missing[:10]}")
        if len(found) != exp_pages:
            errors.append(f"MarkdownReader found {len(found)} != {exp_pages}")

    manifest_path = doc_dir / "manifest.json"
    manifest_images = None
    if manifest_path.is_file():
        m = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest_images = m.get("images_extracted")

    return {
        "doc_id": doc_id,
        "ok": len(errors) == 0,
        "expected_pages": exp_pages,
        "md_files": md_files,
        "md_bytes": md_bytes,
        "zero_byte_md_pages": zero_md_pages,
        "image_files": img_files,
        "image_bytes": img_bytes,
        "manifest_images": manifest_images,
        "errors": errors,
    }


def main():
    t0 = time.time()
    inv = json.loads(INVENTORY_PATH.read_text(encoding="utf-8"))
    docs = inv["documents"]
    with ThreadPoolExecutor(max_workers=16) as pool:
        results = list(pool.map(verify_document, docs))

    failed = [r for r in results if not r["ok"]]
    total_pages = sum(r["expected_pages"] for r in results)
    total_md = sum(r["md_files"] for r in results)
    total_imgs = sum(r["image_files"] for r in results)
    elapsed = round(time.time() - t0, 2)
    report_path = INVENTORY_PATH.parent.parent / "full_291_corpus_reader_verification.json"
    report = {
        "schema": "gptgrep.xl-docbench.full-291-corpus-reader-verification.v1",
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "documents_verified": len(results),
        "expected_pages_total": total_pages,
        "md_files_verified": total_md,
        "images_verified_on_disk": total_imgs,
        "manifest_images_sum": sum(r["manifest_images"] or 0 for r in results),
        "zero_byte_md_pages_total": sum(len(r["zero_byte_md_pages"]) for r in results),
        "failed_docs_count": len(failed),
        "microsoft_native_markdown_reader_all_passed": len(failed) == 0,
        "elapsed_seconds": elapsed,
        "documents": results,
    }
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(
        f"Verified {len(results)} docs in {elapsed:.2f}s: "
        f"md={total_md}/{total_pages}, images={total_imgs}, failed_docs={len(failed)}"
    )
    if failed:
        for f in failed:
            print(f"  FAIL {f['doc_id']}: {len(f['errors'])} errors (e.g. {f['errors'][0]})")
        sys.exit(1)


if __name__ == "__main__":
    main()
