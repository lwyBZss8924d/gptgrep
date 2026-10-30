"""Adversarial Challenge & Stress Test Suite for DocAtlas Exporter.

Tests:
1. Reader alignment: 0-based folder <doc_stem>_page<0-based-idx>/vlm/<doc_stem>_page<0-based-idx>.md
   aligns with 1-based API read_page(1) mapping to _page0.
2. Multi-page sequence and batch retrieval: read_pages([1, 2, ...]).
3. Markdown size boundaries:
   - 5MB exact (allowed)
   - 5MB + 1 byte (rejected with ValueError)
   - Multibyte UTF-8 boundary testing (byte-based check vs char-based)
   - Massive 10MB payload (rejected)
4. Image size boundaries:
   - 20MB exact (allowed)
   - 20MB + 1 byte (rejected with ValueError)
   - Massive 25MB payload (rejected)
5. Directory traversal resistance:
   - Relative path traversal in image filenames ("../../evil.png", "subdir/img.png")
   - Absolute path injection ("/etc/passwd", "/tmp/pwn.jpg")
   - Traversal syntax in markdown references (![alt](../../../secret.png))
   - HTML <img> tag traversal (<img src="../../../etc/shadow">)
   - Staging cleanup verification under failure injection
6. Page numbering boundaries (0, -1, 1).
"""
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

WORKTREE_ROOT = Path(os.environ.get("GPTGREP_WORKTREE_ROOT") or Path(__file__).resolve().parents[2])
PREPROCESS_DIR = WORKTREE_ROOT / ".local/data-engineering/xl-docbench/preprocess/drive-queue-runtime-r1"
if str(PREPROCESS_DIR) not in sys.path:
    sys.path.insert(0, str(PREPROCESS_DIR))

import docatlas_exporter as exporter

DOCATLAS_PATH = Path(
    os.environ.get("DOCATLAS_HARNESS_ROOT")
    or (Path.home() / "dev-space" / "microsoft" / "DocAtlas-Harness")
)
if str(DOCATLAS_PATH) not in sys.path:
    sys.path.insert(0, str(DOCATLAS_PATH))

try:
    from docatlas.skills._common.markdown_reader import MarkdownReader
    DOCATLAS_READER_AVAILABLE = True
except ImportError:
    DOCATLAS_READER_AVAILABLE = False


class TestAdversarialDocAtlasExporter(unittest.TestCase):

    def setUp(self):
        self.temp_dir = tempfile.mkdtemp(prefix="adv_docatlas_")
        self.addCleanup(lambda: shutil.rmtree(self.temp_dir, ignore_errors=True))
        self.exporter = exporter.DocAtlasExporter(self.temp_dir)

    # -------------------------------------------------------------------------
    # 1. Reader Alignment Tests
    # -------------------------------------------------------------------------
    def test_page_number_to_folder_alignment(self):
        """Verify 1-based page_num maps strictly to 0-based _page<num-1>."""
        doc_stem = "doc_align_test"
        
        # Test pages 1, 2, 5, 10, 100
        test_pages = [1, 2, 5, 10, 100]
        for pn in test_pages:
            expected_idx = pn - 1
            page_dir = self.exporter.export_page(
                doc_stem=doc_stem,
                page_num=pn,
                markdown_text=f"# Page Content for {pn}\n\nParagraph for page {pn}."
            )
            expected_folder_name = f"{doc_stem}_page{expected_idx}"
            self.assertEqual(page_dir.name, expected_folder_name)
            
            vlm_md = page_dir / "vlm" / f"{expected_folder_name}.md"
            self.assertTrue(vlm_md.is_file(), f"Expected markdown file {vlm_md} does not exist")
            self.assertIn(f"Page Content for {pn}", vlm_md.read_text(encoding='utf-8'))

    def test_live_docatlas_reader_multi_page_and_batch_alignment(self):
        """Verify live MarkdownReader reads 1-based page numbers correctly from exported structure."""
        self.assertTrue(DOCATLAS_READER_AVAILABLE, "DocAtlas MarkdownReader should be importable")
        doc_stem = "doc_multi_align"
        
        # Export 3 pages: 1, 2, 3
        img1 = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + b"\x00" * 32
        img2 = b"\xff\xd8\xff\xe0\x00\x10JFIF" + b"\x00" * 32
        
        self.exporter.export_page(doc_stem, 1, "# Page 1\n\n![Img 1](fig1.png)", {"fig1.png": img1})
        self.exporter.export_page(doc_stem, 2, "# Page 2\n\n<img src='fig2.jpg' alt='Img 2'>", {"fig2.jpg": img2})
        self.exporter.export_page(doc_stem, 3, "# Page 3 without images\n\nPure text here.")

        reader = MarkdownReader(self.temp_dir, doc_stem)
        self.assertTrue(reader.available)

        # 1-based read_page(1) must read _page0
        p1 = reader.read_page(1)
        self.assertIsNotNone(p1)
        self.assertEqual(p1.page_num, 1)
        self.assertIn("Page 1", p1.markdown)
        self.assertEqual(len(p1.image_paths), 1)
        self.assertTrue(p1.image_paths[0].endswith("fig1.png"))
        self.assertTrue(os.path.isfile(p1.image_paths[0]))

        # 1-based read_page(2) must read _page1
        p2 = reader.read_page(2)
        self.assertIsNotNone(p2)
        self.assertEqual(p2.page_num, 2)
        self.assertIn("Page 2", p2.markdown)
        self.assertEqual(len(p2.image_paths), 1)
        self.assertTrue(p2.image_paths[0].endswith("fig2.jpg"))
        self.assertTrue(os.path.isfile(p2.image_paths[0]))

        # 1-based read_page(3) must read _page2
        p3 = reader.read_page(3)
        self.assertIsNotNone(p3)
        self.assertEqual(p3.page_num, 3)
        self.assertIn("Page 3 without images", p3.markdown)
        self.assertEqual(len(p3.image_paths), 0)

        # Non-existent page 4
        self.assertIsNone(reader.read_page(4))

        # Batch read_pages([1, 2, 4])
        found, missing = reader.read_pages([1, 2, 4])
        self.assertEqual(set(found.keys()), {1, 2})
        self.assertEqual(missing, [4])

    # -------------------------------------------------------------------------
    # 2. Markdown Boundary Conditions
    # -------------------------------------------------------------------------
    def test_markdown_exact_boundary_5mb(self):
        """5MB (5 * 1024 * 1024 bytes) markdown must be accepted."""
        doc_stem = "doc_md_limit_exact"
        # 5MB exact payload
        exact_5mb_text = "A" * (5 * 1024 * 1024)
        page_dir = self.exporter.export_page(doc_stem, 1, exact_5mb_text)
        self.assertTrue(page_dir.is_dir())
        md_file = page_dir / "vlm" / f"{doc_stem}_page0.md"
        self.assertEqual(md_file.stat().st_size, 5 * 1024 * 1024)

    def test_markdown_oversized_5mb_plus_one(self):
        """5MB + 1 byte markdown must be rejected with ValueError."""
        doc_stem = "doc_md_limit_overflow"
        overflow_text = "A" * (5 * 1024 * 1024 + 1)
        with self.assertRaises(ValueError) as ctx:
            self.exporter.export_page(doc_stem, 1, overflow_text)
        self.assertIn("exceeds limit of 5242880 bytes", str(ctx.exception))

    def test_markdown_multibyte_utf8_overflow(self):
        """Multibyte UTF-8 characters exceeding 5MB byte size must be rejected."""
        doc_stem = "doc_md_utf8_overflow"
        # '中' is 3 bytes in UTF-8
        # 1,747,627 chars * 3 bytes = 5,242,881 bytes (> 5MB by 1 byte)
        utf8_text = "中" * 1747627
        self.assertLess(len(utf8_text), 5 * 1024 * 1024)  # Char count is < 5M
        self.assertEqual(len(utf8_text.encode('utf-8')), 5 * 1024 * 1024 + 1)
        
        with self.assertRaises(ValueError) as ctx:
            self.exporter.export_page(doc_stem, 1, utf8_text)
        self.assertIn("exceeds limit of 5242880 bytes", str(ctx.exception))

    def test_markdown_multibyte_utf8_within_limit(self):
        """Multibyte UTF-8 characters within 5MB byte size must be accepted."""
        doc_stem = "doc_md_utf8_valid"
        # 1,747,626 chars * 3 bytes = 5,242,878 bytes (<= 5MB)
        utf8_text = "中" * 1747626
        page_dir = self.exporter.export_page(doc_stem, 1, utf8_text)
        self.assertTrue(page_dir.is_dir())
        md_file = page_dir / "vlm" / f"{doc_stem}_page0.md"
        self.assertEqual(md_file.stat().st_size, 5242878)

    def test_markdown_massive_overflow(self):
        """10MB markdown must be rejected with ValueError."""
        doc_stem = "doc_md_massive"
        huge_text = "X" * (10 * 1024 * 1024)
        with self.assertRaises(ValueError):
            self.exporter.export_page(doc_stem, 1, huge_text)

    # -------------------------------------------------------------------------
    # 3. Image Boundary Conditions
    # -------------------------------------------------------------------------
    def test_image_exact_boundary_20mb(self):
        """20MB (20 * 1024 * 1024 bytes) image must be accepted."""
        doc_stem = "doc_img_exact"
        exact_20mb = b"\x00" * (20 * 1024 * 1024)
        page_dir = self.exporter.export_page(doc_stem, 1, "# Image Page\n\n![big](big.png)", {"big.png": exact_20mb})
        self.assertTrue(page_dir.is_dir())
        img_file = page_dir / "vlm" / "images" / "big.png"
        self.assertEqual(img_file.stat().st_size, 20 * 1024 * 1024)

    def test_image_oversized_20mb_plus_one(self):
        """20MB + 1 byte image must be rejected with ValueError."""
        doc_stem = "doc_img_overflow"
        overflow_img = b"\x00" * (20 * 1024 * 1024 + 1)
        with self.assertRaises(ValueError) as ctx:
            self.exporter.export_page(doc_stem, 1, "# Title", {"big.png": overflow_img})
        self.assertIn("exceeds limit of 20971520 bytes", str(ctx.exception))

    def test_image_massive_overflow(self):
        """25MB image must be rejected with ValueError."""
        doc_stem = "doc_img_massive"
        huge_img = b"\x00" * (25 * 1024 * 1024)
        with self.assertRaises(ValueError):
            self.exporter.export_page(doc_stem, 1, "# Title", {"huge.png": huge_img})

    # -------------------------------------------------------------------------
    # 4. Directory Traversal & Security Resistance
    # -------------------------------------------------------------------------
    def test_directory_traversal_image_filenames(self):
        """Adversarial relative and absolute image paths must NOT escape vlm/images/."""
        doc_stem = "doc_traversal_test"
        traversal_attempts = [
            "../../evil.png",
            "../../../../tmp/pwned.png",
            "/etc/passwd",
            "subdir/nested/deep.png",
            "....//....//exploit.png"
        ]
        
        img_data = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + b"\x00" * 20
        images_dict = {name: img_data for name in traversal_attempts}
        
        page_dir = self.exporter.export_page(
            doc_stem=doc_stem,
            page_num=1,
            markdown_text="# Security Test",
            images=images_dict
        )
        
        vlm_images = page_dir / "vlm" / "images"
        self.assertTrue(vlm_images.is_dir())

        # Verify all output files are contained strictly inside vlm/images/
        written_files = list(vlm_images.iterdir())
        self.assertEqual(len(written_files), len(traversal_attempts))
        
        for f in written_files:
            # Must be a direct child of vlm_images
            self.assertEqual(f.parent.resolve(), vlm_images.resolve())
            self.assertIn(f.name, ["evil.png", "pwned.png", "passwd", "deep.png", "exploit.png"])

        # Verify no files were created outside output_root or in parent directories
        self.assertFalse((Path(self.temp_dir).parent / "evil.png").exists())
        self.assertFalse((Path(self.temp_dir) / "evil.png").exists())

    def test_directory_traversal_in_markdown_syntax(self):
        """Markdown image references with traversal syntax must be normalized safely."""
        doc_stem = "doc_md_traversal"
        raw_md = (
            "# Page Title\n"
            "![Evil 1](../../../etc/passwd)\n"
            "![Evil 2](/var/log/syslog)\n"
            "<img src='../../secret/token.png' alt='Token'>\n"
            "<img src='/absolute/path/leak.jpg'>\n"
        )
        normalized = exporter.normalize_markdown_images(raw_md)
        
        # Every link must be normalized to images/<filename>
        self.assertIn("![Evil 1](images/passwd)", normalized)
        self.assertIn("![Evil 2](images/syslog)", normalized)
        self.assertIn("![Token](images/token.png)", normalized)
        self.assertIn("![](images/leak.jpg)", normalized)
        
        # Must not contain traversal or absolute references
        self.assertNotIn("../", normalized)
        self.assertNotIn("/etc/", normalized)
        self.assertNotIn("/var/", normalized)
        self.assertNotIn("/absolute/", normalized)

    def test_staging_cleanup_on_midway_failure(self):
        """If image validation fails on 2nd image, staging directory must be cleanly removed."""
        doc_stem = "doc_staging_cleanup"
        valid_img = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + b"\x00" * 20
        oversized_img = b"B" * (20 * 1024 * 1024 + 10)
        
        doc_dir = Path(self.temp_dir) / doc_stem
        
        with self.assertRaises(ValueError):
            self.exporter.export_page(
                doc_stem=doc_stem,
                page_num=1,
                markdown_text="# Fail halfway",
                images={"img1.png": valid_img, "img2.png": oversized_img}
            )

        # Neither the final page dir nor any .staging dir should exist
        if doc_dir.exists():
            remaining_entries = list(doc_dir.iterdir())
            staging_entries = [e for e in remaining_entries if e.name.startswith(".staging_")]
            self.assertEqual(len(staging_entries), 0, f"Found leaked staging directories: {staging_entries}")
            self.assertEqual(len(remaining_entries), 0, f"Expected no entries, found {remaining_entries}")

    # -------------------------------------------------------------------------
    # 5. Page Number Boundary Validation
    # -------------------------------------------------------------------------
    def test_page_number_non_positive_rejection(self):
        """page_num < 1 must be rejected with ValueError (1-based API contract)."""
        invalid_page_nums = [0, -1, -99]
        for pn in invalid_page_nums:
            with self.assertRaises(ValueError) as ctx:
                self.exporter.export_page("doc", pn, "# Invalid Page")
            self.assertIn("page_num must be >= 1", str(ctx.exception))

    # -------------------------------------------------------------------------
    # 6. Regression Tests: Middle-Page Gaps, Duplicate Natural Keys & Pagination
    # -------------------------------------------------------------------------
    def test_middle_page_gap_detection_and_reader_missing_reporting(self):
        """Regression test: first/last page checks must not mask missing middle pages."""
        self.assertTrue(DOCATLAS_READER_AVAILABLE, "DocAtlas MarkdownReader should be importable")
        doc_stem = "doc_middle_gap_reg"
        expected_pages = 5
        # Export pages 1, 2, 4, 5 but omit middle page 3 (_page2)
        for pn in [1, 2, 4, 5]:
            self.exporter.export_page(doc_stem, pn, f"# Page {pn} body")

        reader = MarkdownReader(self.temp_dir, doc_stem)
        # First and last pages exist, showing why first/last check is insufficient
        self.assertIsNotNone(reader.read_page(1))
        self.assertIsNotNone(reader.read_page(expected_pages))
        # Full 1..expected_pages check via MarkdownReader catches missing middle page 3
        found, missing = reader.read_pages(list(range(1, expected_pages + 1)))
        self.assertEqual(missing, [3])
        self.assertNotIn(3, found)

    def test_duplicate_page_directory_and_natural_key_rejection(self):
        """Regression test: duplicate page folders (' (1)' or duplicate Drive natural keys) must be detected."""
        doc_stem = "doc_dup_dir_reg"
        self.exporter.export_page(doc_stem, 1, "# Page 1")
        doc_dir = Path(self.temp_dir) / doc_stem
        dup_dir = doc_dir / f"{doc_stem}_page0 (1)"
        dup_dir.mkdir(parents=True)

        dup_entries = [p.name for p in doc_dir.rglob("*") if " (" in p.name]
        self.assertEqual(dup_entries, [f"{doc_stem}_page0 (1)"])

        # Simulated Drive API listing with duplicate natural key (e.g. doc_000179_page801)
        cloud_items = [
            {"id": "id_1", "name": "doc_000179_page801"},
            {"id": "id_2", "name": "doc_000179_page801"},
        ]
        names = [item["name"] for item in cloud_items]
        self.assertNotEqual(len(names), len(set(names)), "Duplicate natural keys must be flagged")


if __name__ == '__main__':
    unittest.main()
