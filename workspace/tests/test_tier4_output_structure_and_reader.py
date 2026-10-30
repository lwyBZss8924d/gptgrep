"""Tier 4 Test: DocAtlas Reader Compatibility and Output Structure."""
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
if str(DOCATLAS_PATH) not in sys.path:
    sys.path.insert(0, str(DOCATLAS_PATH))

try:
    from docatlas.skills._common.markdown_reader import MarkdownReader
    DOCATLAS_READER_AVAILABLE = True
except ImportError:
    DOCATLAS_READER_AVAILABLE = False


class TestTier4OutputStructureAndReader(unittest.TestCase):

    def setUp(self):
        self.temp_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: shutil.rmtree(self.temp_dir, ignore_errors=True))

    def test_docatlas_per_page_folder_and_file_layout(self):
        exp = exporter.DocAtlasExporter(self.temp_dir)
        doc_id = "doc_000007"
        
        # 1-based page 1 -> doc_000007_page0
        # 1-based page 2 -> doc_000007_page1
        img_payload = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + b"\x00" * 40
        exp.export_page(doc_id, 1, "# Page 1\n\n![Figure](fig1.png)", {"fig1.png": img_payload})
        exp.export_page(doc_id, 2, "# Page 2\n\n<img src='fig2.png' alt='Fig 2'>", {"fig2.png": img_payload})

        doc_dir = Path(self.temp_dir) / doc_id
        self.assertTrue(doc_dir.is_dir())

        p0_dir = doc_dir / f"{doc_id}_page0" / "vlm"
        p1_dir = doc_dir / f"{doc_id}_page1" / "vlm"
        self.assertTrue(p0_dir.is_dir())
        self.assertTrue(p1_dir.is_dir())

        self.assertTrue((p0_dir / f"{doc_id}_page0.md").is_file())
        self.assertTrue((p1_dir / f"{doc_id}_page1.md").is_file())
        self.assertTrue((p0_dir / "images" / "fig1.png").is_file())
        self.assertTrue((p1_dir / "images" / "fig2.png").is_file())

    def test_live_docatlas_markdown_reader_compatibility(self):
        self.assertTrue(DOCATLAS_READER_AVAILABLE, "DocAtlas MarkdownReader must be importable")
        exp = exporter.DocAtlasExporter(self.temp_dir)
        doc_id = "doc_000008"

        img_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + b"\x00" * 40
        exp.export_page(doc_id, 1, "# Doc 8 Page 1\n\n![Chart](chart.png)", {"chart.png": img_bytes})

        reader = MarkdownReader(self.temp_dir, doc_id)
        self.assertTrue(reader.available)

        page1 = reader.read_page(1)
        self.assertIsNotNone(page1)
        self.assertEqual(page1.page_num, 1)
        self.assertIn("Doc 8 Page 1", page1.markdown)
        self.assertEqual(len(page1.image_paths), 1)
        self.assertTrue(os.path.isfile(page1.image_paths[0]))

        # Reading non-existent page 99 returns None
        self.assertIsNone(reader.read_page(99))

    def test_empirical_physical_drive_artifacts_when_present(self):
        ocr_root = _resolve_drive_root() / "XL-DocBench-OCR-20261018"
        if not ocr_root.exists():
            self.skipTest(f"Empirical OCR root {ocr_root} not available on this host")
        import verify_m2_empirical_artifacts
        summaries = verify_m2_empirical_artifacts.run_checks()
        self.assertEqual(len(summaries), 4)
        total_pages = sum(s['verified_pages'] for s in summaries.values())
        self.assertIn(total_pages, (437, 693))
        total_images = sum(s['images_extracted'] for s in summaries.values())
        self.assertIn(total_images, (138, 406))


if __name__ == '__main__':
    unittest.main()
