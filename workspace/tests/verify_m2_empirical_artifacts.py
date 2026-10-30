#!/usr/bin/env python3
"""Empirical Verification Script for Milestone 2 DocAtlas & Structure Artifacts.

Performs deep empirical validation on physical outputs in:
$XL_DOCBENCH_DRIVE_ROOT/XL-DocBench-OCR-20261018 (or local CloudStorage mount)

Verifies:
1. Exact page count completeness:
   - doc_000166: 1 page
   - doc_000001: 2 pages
   - doc_000008: 52 pages
   - doc_000007: 382 pages
   Total = 437 pages
2. Manifest and source-map integrity & consistency for each document
3. Markdown bounds: <= 5MB (5,242,880 bytes)
4. Image bounds: <= 20MB (20,971,520 bytes)
5. Physical presence of all image files referenced in markdown
6. Standard DocAtlas layout:
   <doc_id>/<doc_id>_page<i>/vlm/<doc_id>_page<i>.md
   <doc_id>/<doc_id>_page<i>/vlm/<doc_id>_page<i>_middle.json
   <doc_id>/<doc_id>_page<i>/vlm/<doc_id>_page<i>_model.json
   <doc_id>/<doc_id>_page<i>/vlm/<doc_id>_page<i>_structured_content.json
   <doc_id>/<doc_id>_page<i>/vlm/images/<filename>
7. Live Microsoft DocAtlas MarkdownReader integration test across all documents.
"""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import re
import sys
import time
import unittest

def _resolve_drive_root() -> Path:
    env_root = os.environ.get("XL_DOCBENCH_DRIVE_ROOT")
    if env_root:
        return Path(env_root)
    cs_dir = Path.home() / "Library" / "CloudStorage"
    if cs_dir.is_dir():
        matches = sorted(cs_dir.glob("GoogleDrive-*"))
        if matches:
            candidate = matches[0] / "My Drive"
            if candidate.exists():
                return candidate
    return Path(os.environ.get("XL_DOCBENCH_CORPUS_ROOT", "/tmp/xl-docbench"))


OCR_ROOT = _resolve_drive_root() / "XL-DocBench-OCR-20261018"
DOCATLAS_PATH = Path(
    os.environ.get("DOCATLAS_HARNESS_ROOT")
    or (Path.home() / "dev-space" / "microsoft" / "DocAtlas-Harness")
)

MAX_MD_BYTES = 5 * 1024 * 1024  # 5 MB
MAX_IMG_BYTES = 20 * 1024 * 1024  # 20 MB

EXPECTED_DOCS_M2 = {
    'doc_000166': 1,
    'doc_000001': 2,
    'doc_000008': 52,
    'doc_000007': 382,
}

EXPECTED_DOCS_M3 = {
    'doc_000166': 55,
    'doc_000001': 204,
    'doc_000008': 52,
    'doc_000007': 382,
}

EXPECTED_DOCS = EXPECTED_DOCS_M3

MD_IMG_PATTERN = re.compile(r'!\[([^\]]*)\]\(([^)]+)\)')
HTML_IMG_PATTERN = re.compile(r'<img\s+[^>]*src=[\'"]([^\'"]+)[\'"][^>]*>', re.IGNORECASE)


def verify_single_page(args):
    doc_id, page_idx, doc_dir = args
    page_folder_name = f"{doc_id}_page{page_idx}"
    page_dir = doc_dir / page_folder_name
    if not page_dir.is_dir():
        return False, f"Missing page directory: {page_dir}", 0, 0, 0

    vlm_dir = page_dir / 'vlm'
    if not vlm_dir.is_dir():
        return False, f"Missing vlm directory: {vlm_dir}", 0, 0, 0

    md_path = vlm_dir / f"{page_folder_name}.md"
    if not md_path.is_file():
        return False, f"Missing markdown file: {md_path}", 0, 0, 0

    md_size = md_path.stat().st_size
    if md_size > MAX_MD_BYTES:
        return False, f"Markdown file {md_path} exceeds 5MB limit: {md_size} > {MAX_MD_BYTES}", 0, 0, 0

    # Companion files
    for comp_name in ['middle.json', 'model.json', 'structured_content.json']:
        comp_file = vlm_dir / f"{page_folder_name}_{comp_name}"
        if not comp_file.is_file():
            return False, f"Missing companion file: {comp_file}", 0, 0, 0

    with open(md_path, 'r', encoding='utf-8') as f:
        md_content = f.read()

    ref_images = []
    for m in MD_IMG_PATTERN.finditer(md_content):
        ref_images.append(m.group(2))
    for m in HTML_IMG_PATTERN.finditer(md_content):
        ref_images.append(m.group(1))

    images_dir = vlm_dir / 'images'
    if ref_images and not images_dir.is_dir():
        return False, f"Images dir missing when md references images: {images_dir}", 0, 0, 0

    img_count = 0
    img_bytes = 0

    for img_ref in ref_images:
        if img_ref.startswith('/'):
            return False, f"Absolute image ref forbidden: {img_ref} in {md_path}", 0, 0, 0
        if '..' in img_ref:
            return False, f"Path traversal forbidden: {img_ref} in {md_path}", 0, 0, 0
        clean_ref = img_ref
        if clean_ref.startswith('images/'):
            clean_ref = clean_ref[len('images/'):]
        physical_img = images_dir / clean_ref
        if not physical_img.is_file():
            return False, f"Referenced image {img_ref} physically missing at {physical_img}", 0, 0, 0

    if images_dir.is_dir():
        for img_file in images_dir.iterdir():
            if img_file.name.startswith('.'):
                continue
            isz = img_file.stat().st_size
            if isz > MAX_IMG_BYTES:
                return False, f"Image {img_file} exceeds 20MB limit: {isz} > {MAX_IMG_BYTES}", 0, 0, 0
            if isz == 0:
                return False, f"Image {img_file} is 0 bytes empty!", 0, 0, 0
            img_count += 1
            img_bytes += isz

    return True, "", 1, md_size, img_count, img_bytes


def run_checks():
    t0 = time.time()
    print(f"=== Starting Empirical Verification on {OCR_ROOT} ===")
    assert OCR_ROOT.exists(), f"OCR root path {OCR_ROOT} does not exist!"

    total_pages_verified = 0
    total_images_verified = 0
    total_md_bytes = 0
    total_img_bytes = 0

    doc_summaries = {}

    expected_docs = EXPECTED_DOCS
    expected_total_pages = sum(expected_docs.values())

    for doc_id, expected_pages in expected_docs.items():
        doc_dir = OCR_ROOT / doc_id
        assert doc_dir.is_dir(), f"Document directory {doc_dir} not found!"

        # 1. Manifest verification
        manifest_path = doc_dir / 'manifest.json'
        assert manifest_path.is_file(), f"Manifest missing: {manifest_path}"
        with open(manifest_path, 'r', encoding='utf-8') as f:
            manifest = json.load(f)
        assert manifest.get('doc_id') == doc_id, f"Manifest doc_id mismatch: {manifest.get('doc_id')} != {doc_id}"
        pages_processed = manifest.get('pages_processed')
        assert pages_processed == expected_pages, (
            f"Manifest pages_processed mismatch for {doc_id}: {pages_processed} != {expected_pages}"
        )

        # 2. Source-map verification
        sourcemap_path = doc_dir / 'source-map.json'
        assert sourcemap_path.is_file(), f"Source-map missing: {sourcemap_path}"
        with open(sourcemap_path, 'r', encoding='utf-8') as f:
            sourcemap = json.load(f)
        assert sourcemap.get('doc_id') == doc_id, f"Sourcemap doc_id mismatch: {sourcemap.get('doc_id')} != {doc_id}"
        pages_map = sourcemap.get('pages', [])
        assert len(pages_map) == expected_pages, (
            f"Sourcemap pages count mismatch for {doc_id}: {len(pages_map)} != {expected_pages}"
        )

        # 3. Parallel verification of all pages
        tasks = [(doc_id, idx, doc_dir) for idx in range(expected_pages)]
        doc_md_bytes = 0
        doc_img_bytes = 0
        doc_img_count = 0

        with ThreadPoolExecutor(max_workers=16) as pool:
            results = list(pool.map(verify_single_page, tasks))

        for ok, err, pcount, md_size, icount, ibytes in results:
            assert ok, f"Page verification failed in {doc_id}: {err}"
            doc_md_bytes += md_size
            doc_img_count += icount
            doc_img_bytes += ibytes

        total_pages_verified += expected_pages
        total_images_verified += doc_img_count
        total_md_bytes += doc_md_bytes
        total_img_bytes += doc_img_bytes

        doc_summaries[doc_id] = {
            'expected_pages': expected_pages,
            'verified_pages': expected_pages,
            'images_extracted': doc_img_count,
            'manifest_images': manifest.get('images_extracted'),
            'total_md_bytes': doc_md_bytes,
            'total_img_bytes': doc_img_bytes,
        }
        print(f"[{doc_id}] OK: {expected_pages} pages, {doc_img_count} images (manifest says {manifest.get('images_extracted')}), "
              f"MD: {doc_md_bytes / 1024:.1f} KB, IMG: {doc_img_bytes / (1024 * 1024):.2f} MB")

    print("\n=== Page Count & Bounds Verification Summary ===")
    print(f"Total Documents: {len(expected_docs)}")
    print(f"Total Pages Verified: {total_pages_verified} / {expected_total_pages}")
    assert total_pages_verified == expected_total_pages, f"Total pages mismatch: {total_pages_verified} != {expected_total_pages}"
    print(f"Total Physical Images Verified: {total_images_verified}")
    print(f"Total Markdown Size: {total_md_bytes / (1024 * 1024):.2f} MB (all <= 5MB per page)")
    print(f"Total Images Size: {total_img_bytes / (1024 * 1024):.2f} MB (all <= 20MB per image)")

    # 4. DocAtlas MarkdownReader Integration Verification
    print("\n=== Live DocAtlas MarkdownReader Testing ===")
    if str(DOCATLAS_PATH) not in sys.path:
        sys.path.insert(0, str(DOCATLAS_PATH))
    from docatlas.skills._common.markdown_reader import MarkdownReader

    for doc_id, pages_count in expected_docs.items():
        reader = MarkdownReader(str(OCR_ROOT), doc_id)
        assert reader.available, f"Reader unavailable for {doc_id}"

        # Test first page (1-based index)
        p1 = reader.read_page(1)
        assert p1 is not None, f"Failed to read page 1 of {doc_id}"
        assert p1.page_num == 1, f"Page number mismatch for {doc_id} page 1"

        # Test last page
        plast = reader.read_page(pages_count)
        assert plast is not None, f"Failed to read page {pages_count} of {doc_id}"
        assert plast.page_num == pages_count, f"Page number mismatch for {doc_id} last page"

        # Test out-of-bounds page
        p_invalid = reader.read_page(pages_count + 1)
        assert p_invalid is None, f"Expected None for out-of-bounds page {pages_count + 1} in {doc_id}"

        # Test batch read
        test_indices = [1, pages_count]
        if pages_count > 2:
            test_indices.append(pages_count // 2)
        found, missing = reader.read_pages(test_indices + [999999])
        for idx in test_indices:
            assert idx in found, f"Batch read failed to find page {idx} in {doc_id}"
        assert 999999 in missing, f"Batch read failed to report missing page 999999 in {doc_id}"

        print(f"[{doc_id}] MarkdownReader verified: Page 1 ({len(p1.markdown)} chars, {len(p1.image_paths)} imgs), "
              f"Page {pages_count} ({len(plast.markdown)} chars, {len(plast.image_paths)} imgs)")

    elapsed = round(time.time() - t0, 2)
    print(f"\nALL {expected_total_pages} PAGES EMPIRICALLY VERIFIED IN {elapsed}s!")
    return doc_summaries


class TestM2EmpiricalArtifacts(unittest.TestCase):
    def test_empirical_artifacts_and_reader(self):
        if not OCR_ROOT.exists():
            self.skipTest(f"Physical OCR root {OCR_ROOT} does not exist on this machine")
        summaries = run_checks()
        self.assertEqual(len(summaries), 4)
        total_pages = sum(s['verified_pages'] for s in summaries.values())
        self.assertIn(total_pages, (437, 693))
        total_images = sum(s['images_extracted'] for s in summaries.values())
        self.assertIn(total_images, (138, 406))


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--unittest':
        unittest.main(argv=[sys.argv[0]] + sys.argv[2:])
    else:
        run_checks()

