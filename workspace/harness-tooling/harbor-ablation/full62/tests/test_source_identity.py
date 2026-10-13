"""The suite must exercise this checkout's adapter source, not another installed copy."""
from pathlib import Path
import unittest

import gptgrep_harbor


class SourceIdentityTests(unittest.TestCase):
    def test_suite_imports_the_adjacent_source_tree(self):
        # A shared environment's editable install can point at another
        # checkout; then every other module here would test that code instead.
        expected = (Path(__file__).resolve().parents[1] / "src" / "gptgrep_harbor").resolve()
        imported = Path(gptgrep_harbor.__file__).resolve().parent
        self.assertEqual(imported, expected,
                         "the suite imported another gptgrep_harbor; set PYTHONPATH to this checkout's src")


if __name__ == "__main__":
    unittest.main()
