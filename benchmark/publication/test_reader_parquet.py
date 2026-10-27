import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import pyarrow as pa
import pyarrow.parquet as pq

sys.path.insert(0, str(Path(__file__).parent))
import reader_parquet as adapter


def task():
    return {"id": "synthetic", "taskType": "single_doc", "contextDocumentIds": ["source"], "contextId": "context",
            "groupId": "group", "question": "Unicode 漢字 😀 e\u0301 separator\u2028 / slash"}


class ReaderParquetTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="reader-parquet-")
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def test_real_unicode_roundtrip_and_metadata_free_writer(self):
        source, output = self.root / "safe.jsonl", self.root / "safe.parquet"
        rows = [task()]
        source.write_bytes((adapter.canonical(rows[0]) + "\n").encode())
        receipt = adapter.write(source, output, "single_doc")
        decoded = adapter.decode(output, "single_doc")
        self.assertEqual(decoded["rows"], rows)
        self.assertEqual(receipt["rows"], 1)
        self.assertEqual(decoded["fileMetadata"], {})
        self.assertEqual(decoded["schemaMetadata"], {})
        self.assertTrue(all(not values for values in decoded["fieldMetadata"].values()))
        self.assertEqual(decoded["fields"], adapter.TASK_FIELDS)

    def test_empty_table_still_has_exact_physical_schema(self):
        source, output = self.root / "empty.jsonl", self.root / "empty.parquet"
        source.write_text("")
        adapter.write(source, output, "cross_doc")
        self.assertEqual(adapter.decode(output, "cross_doc")["rows"], [])
        extra = pa.schema([*adapter.schema_for("cross_doc"), pa.field("gold", pa.string())])
        bad = self.root / "extra.parquet"
        pq.write_table(pa.Table.from_pylist([], schema=extra), bad, store_schema=False)
        with self.assertRaisesRegex(adapter.ContractError, "physical_parquet_schema"):
            adapter.decode(bad, "cross_doc")

    def test_duplicate_physical_fields_fail_before_record_conversion(self):
        for size in (0, 1):
            bad = self.root / f"duplicate-{size}.parquet"
            table = pa.Table.from_arrays([pa.array(["first"] * size), pa.array(["second"] * size)], names=["question", "question"])
            pq.write_table(table, bad, store_schema=False)
            with self.assertRaisesRegex(adapter.ContractError, "duplicate_physical_parquet_field"):
                adapter.decode(bad, "single_doc")

    def test_nested_names_and_metadata_are_inspected_recursively(self):
        duplicate = pa.schema([pa.field("outer", pa.struct([pa.field("same", pa.string()), pa.field("same", pa.string())]))])
        with self.assertRaisesRegex(adapter.ContractError, "duplicate_arrow_field_name"):
            adapter.inspect_arrow_fields(duplicate)
        nested = pa.schema([pa.field("contextDocumentIds", pa.list_(pa.field("element", pa.string(), metadata={b"private": b"hidden"})))])
        fields, metadata = adapter.inspect_arrow_fields(nested)
        self.assertEqual(fields, {"contextDocumentIds": "list<string>"})
        self.assertEqual(metadata["contextDocumentIds.element"], {"private": "hidden"})
        dotted = pa.schema([pa.field("contextDocumentIds.element", pa.string()), pa.field("contextDocumentIds", pa.list_(pa.field("element", pa.string())))])
        with self.assertRaisesRegex(adapter.ContractError, "ambiguous_arrow_field_path"):
            adapter.inspect_arrow_fields(dotted)

    def test_file_and_arrow_schema_metadata_are_not_silently_dropped(self):
        for name, store_schema in [("footer", False), ("arrow", True)]:
            path = self.root / f"{name}.parquet"
            schema = adapter.schema_for("single_doc")
            with pq.ParquetWriter(path, schema, store_schema=store_schema) as writer:
                if name == "footer":
                    writer.add_key_value_metadata({"pandas": "private attributes"})
                writer.write_table(pa.Table.from_pylist([task()], schema=schema))
            self.assertIn(b"pandas" if name == "footer" else b"ARROW:schema", pq.read_metadata(path).metadata)
            with self.assertRaisesRegex(adapter.ContractError, "metadata_must_be_empty"):
                adapter.decode(path, "single_doc")

    def test_writer_rejects_duplicate_keys_and_noncanonical_bytes(self):
        canonical = adapter.canonical(task()) + "\n"
        examples = [
            canonical.replace('"id":"synthetic"', '"id":"hidden","id":"synthetic"'),
            canonical.replace("/", "\\u002f"),
            canonical.replace("\n", "\r\n"),
        ]
        for index, value in enumerate(examples):
            source = self.root / f"bad-{index}.jsonl"
            source.write_bytes(value.encode())
            with self.assertRaises(adapter.ContractError):
                adapter.write(source, self.root / f"bad-{index}.parquet", "single_doc")

    def test_external_column_paths_fail_before_any_data_read(self):
        source, external = self.root / "normal.parquet", self.root / "external.parquet"
        pq.write_table(pa.Table.from_pylist([task()], schema=adapter.schema_for("single_doc")), source, store_schema=False)
        metadata = pq.read_metadata(source)
        metadata.set_file_path("untrusted-external-data.parquet")
        metadata.write_metadata_file(external)
        with self.assertRaisesRegex(adapter.ContractError, "external_column_or_column_metadata_denied"):
            adapter.decode(external, "single_doc")

    def test_writer_rejects_nested_judge_fields_and_unsafe_integers(self):
        with self.assertRaisesRegex(adapter.ContractError, "unknown_or_missing_row_fields"):
            adapter.validate_rows([{**task(), "metadata": {"evidence": [1]}}], "single_doc")
        document = {"documentId": "source", "title": None, "url": "https://example.invalid/source.pdf", "sourceHost": "example.invalid",
                    "pageCount": 1, "fileSizeBytes": 10, "domain": "synthetic", "docType": "report", "pdfType": "text"}
        for value in (True, 2**53, -1):
            with self.assertRaisesRegex(adapter.ContractError, "safe_integer_field_required"):
                adapter.validate_rows([{**document, "pageCount": value}], "document")

    def test_compressed_and_dictionary_expansion_are_bounded(self):
        path = self.root / "expanded.parquet"
        rows = [{**task(), "id": f"synthetic-{index}", "question": "x" * 1000} for index in range(30)]
        pq.write_table(pa.Table.from_pylist(rows, schema=adapter.schema_for("single_doc")), path, store_schema=False)
        with patch.object(adapter, "MAX_BYTES", 100):
            with self.assertRaisesRegex(adapter.ContractError, "uncompressed_parquet_limit_exceeded"):
                adapter.decode(path, "single_doc")
        with patch.object(adapter, "MAX_BYTES", 10_000):
            with self.assertRaisesRegex(adapter.ContractError, "expanded_rows_limit_exceeded"):
                adapter.decode(path, "single_doc")


if __name__ == "__main__":
    unittest.main()
