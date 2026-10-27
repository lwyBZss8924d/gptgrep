"""Pinned local Parquet I/O for the closed public reader projection.

The CLI's decode output is for a controller pipe, never a human-facing log.
No upstream answer/evidence fields are accepted by the writer.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import sys

import pyarrow as pa
import pyarrow.parquet as pq

VERSION = "26.0.0"
MAX_BYTES = 64 * 1024 * 1024
MAX_ROWS = 100_000
MAX_SAFE_INTEGER = 2**53 - 1
DOCUMENT_FIELDS = {"documentId": "string", "title": "string", "url": "string", "sourceHost": "string", "pageCount": "integer",
                   "fileSizeBytes": "integer", "domain": "string", "docType": "string", "pdfType": "string"}
TASK_FIELDS = {"id": "string", "taskType": "string", "contextDocumentIds": "list<string>", "contextId": "string", "groupId": "string", "question": "string"}


class ContractError(ValueError):
    pass


def canonical(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def exact_object(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ContractError("duplicate_json_key")
        result[key] = value
    return result


def file_path(value: str, *, existing: bool = True) -> Path:
    path = Path(value)
    if not path.is_absolute():
        raise ContractError("absolute_local_path_required")
    for part in [path, *path.parents]:
        if part.is_symlink():
            raise ContractError("symlink_input_or_output_denied")
    if existing and (not path.is_file() or path.stat().st_size > MAX_BYTES):
        raise ContractError("bounded_regular_input_required")
    return path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def runtime_identity() -> dict:
    if pa.__version__ != VERSION or sys.version_info[:2] != (3, 13):
        raise ContractError("runtime_version_mismatch")
    distribution = importlib.metadata.distribution("pyarrow")
    root = Path(sys.prefix).resolve(strict=True)
    files = []
    for item in sorted(distribution.files or [], key=str):
        path = Path(distribution.locate_file(item))
        if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root):
            raise ContractError("package_file_escaped_environment")
        files.append({"path": str(item), "sha256": sha256_file(path), "bytes": path.stat().st_size})
    if not files:
        raise ContractError("package_inventory_unavailable")
    return {"schema": "gptgrep.pyarrow-runtime.v1", "pythonVersion": platform.python_version(),
            "pythonExecutable": str(Path(sys.executable).absolute()), "pythonRealPath": str(Path(sys.executable).resolve(strict=True)),
            "pythonSha256": sha256_file(Path(sys.executable)), "environmentRoot": str(root), "pyarrowVersion": pa.__version__,
            "packageFiles": len(files), "packageTreeSha256": hashlib.sha256(canonical(files).encode()).hexdigest()}


def assert_runtime(receipt: str) -> None:
    expected = json.loads(file_path(receipt).read_text(), object_pairs_hook=exact_object)
    if expected != runtime_identity():
        raise ContractError("runtime_receipt_changed")


def expected_fields(kind: str) -> dict[str, str]:
    if kind not in ("document", "single_doc", "cross_doc"):
        raise ContractError("unknown_row_kind")
    return DOCUMENT_FIELDS if kind == "document" else TASK_FIELDS


def validate_rows(rows: list, kind: str) -> None:
    fields = expected_fields(kind)
    if not isinstance(rows, list) or len(rows) > MAX_ROWS:
        raise ContractError("bounded_rows_required")
    ids = set()
    for row in rows:
        if not isinstance(row, dict) or set(row) != set(fields):
            raise ContractError("unknown_or_missing_row_fields")
        for name, field_type in fields.items():
            value = row[name]
            if name == "title" and value is None:
                continue
            if field_type == "string" and not isinstance(value, str):
                raise ContractError("string_field_required")
            if field_type == "integer" and (type(value) is not int or not 0 <= value <= MAX_SAFE_INTEGER):
                raise ContractError("safe_integer_field_required")
            if field_type == "list<string>" and (not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value)):
                raise ContractError("string_list_field_required")
        identity = row["documentId" if kind == "document" else "id"]
        if not identity or identity in ids:
            raise ContractError("duplicate_or_empty_row_id")
        ids.add(identity)
        if kind == "document":
            if row["pageCount"] < 1:
                raise ContractError("positive_page_count_required")
        else:
            ids_in_context = row["contextDocumentIds"]
            if row["taskType"] != kind or len(set(ids_in_context)) != len(ids_in_context) or \
                    (kind == "single_doc" and len(ids_in_context) != 1) or (kind == "cross_doc" and len(ids_in_context) < 2):
                raise ContractError("complete_task_context_required")


def schema_for(kind: str) -> pa.Schema:
    types = {"string": pa.string(), "integer": pa.int64(), "list<string>": pa.list_(pa.field("element", pa.string(), nullable=False))}
    return pa.schema([pa.field(name, types[field_type], nullable=name == "title") for name, field_type in expected_fields(kind).items()])


def metadata_strings(metadata: dict | None) -> dict[str, str]:
    # UTF-8 is required so decoding cannot silently drop opaque metadata bytes.
    try:
        return {key.decode("utf-8"): value.decode("utf-8") for key, value in (metadata or {}).items()}
    except (UnicodeError, AttributeError) as error:
        raise ContractError("opaque_metadata_denied") from error


def inspect_arrow_fields(schema: pa.Schema) -> tuple[dict, dict]:
    fields, metadata = {}, {}

    def visit(siblings, prefix: str = "") -> None:
        names = [siblings[index].name for index in range(len(siblings))]
        if len(set(names)) != len(names):
            raise ContractError("duplicate_arrow_field_name")
        for index in range(len(siblings)):
            field = siblings[index]
            path = f"{prefix}.{field.name}" if prefix else field.name
            if path in metadata:
                raise ContractError("ambiguous_arrow_field_path")
            metadata[path] = metadata_strings(field.metadata)
            if pa.types.is_struct(field.type):
                visit(field.type, path)
            elif pa.types.is_list(field.type) or pa.types.is_large_list(field.type) or pa.types.is_fixed_size_list(field.type):
                visit([field.type.value_field], path)
            elif pa.types.is_map(field.type):
                visit([field.type.key_field, field.type.item_field], path)

    visit(schema)
    for index in range(len(schema)):
        field = schema[index]
        if pa.types.is_string(field.type):
            kind = "string"
        elif pa.types.is_int64(field.type):
            kind = "integer"
        elif pa.types.is_list(field.type) and pa.types.is_string(field.type.value_type):
            kind = "list<string>"
        else:
            raise ContractError("unsupported_arrow_field_type")
        fields[field.name] = kind
    return fields, metadata


def inspect_parquet(parquet: pq.ParquetFile, kind: str) -> dict:
    fields = expected_fields(kind)
    # This pinned PyArrow API exposes physical segment vectors before any row conversion.
    physical = [tuple(path) for path in parquet.reader.column_paths]
    if len(set(physical)) != len(physical):
        raise ContractError("duplicate_physical_parquet_field")
    wanted = [(name, "list", "element") if field_type == "list<string>" else (name,) for name, field_type in fields.items()]
    if set(physical) != set(wanted) or len(physical) != len(wanted):
        raise ContractError("physical_parquet_schema_mismatch")
    if parquet.metadata.num_rows > MAX_ROWS or parquet.metadata.created_by != f"parquet-cpp-arrow version {VERSION}":
        raise ContractError("unqualified_parquet_writer_or_row_count")
    uncompressed_bytes = 0
    for group in range(parquet.metadata.num_row_groups):
        for column in range(parquet.metadata.row_group(group).num_columns):
            chunk = parquet.metadata.row_group(group).column(column)
            if chunk.file_path or chunk.metadata:
                raise ContractError("external_column_or_column_metadata_denied")
            if chunk.total_uncompressed_size < 0:
                raise ContractError("invalid_uncompressed_size")
            uncompressed_bytes += chunk.total_uncompressed_size
            if uncompressed_bytes > MAX_BYTES:
                raise ContractError("uncompressed_parquet_limit_exceeded")
    arrow_fields, nested_metadata = inspect_arrow_fields(parquet.schema_arrow)
    if arrow_fields != fields:
        raise ContractError("effective_arrow_schema_mismatch")
    return {"fields": arrow_fields, "fileMetadata": metadata_strings(parquet.metadata.metadata),
            "schemaMetadata": metadata_strings(parquet.schema_arrow.metadata), "fieldMetadata": nested_metadata}


def decode(path: Path, kind: str) -> dict:
    with path.open("rb") as stream:
        parquet = pq.ParquetFile(stream, memory_map=False, pre_buffer=False)
        inspected = inspect_parquet(parquet, kind)
        if inspected["fileMetadata"] or inspected["schemaMetadata"] or any(inspected["fieldMetadata"].values()):
            raise ContractError("parquet_metadata_must_be_empty")
        rows, decoded_bytes = [], 2
        # A dictionary can repeat a large value many times despite a small footer estimate.
        # Convert one record at a time and bound expanded JSON before retaining the next.
        for batch in parquet.iter_batches(batch_size=1, use_threads=False):
            if batch.nbytes > MAX_BYTES:
                raise ContractError("expanded_batch_limit_exceeded")
            for row in batch.to_pylist():
                decoded_bytes += len(canonical(row).encode("utf-8")) + 1
                if decoded_bytes > MAX_BYTES:
                    raise ContractError("expanded_rows_limit_exceeded")
                rows.append(row)
    validate_rows(rows, kind)
    return {"rows": rows, **inspected}


def write(jsonl: Path, output: Path, kind: str) -> dict:
    source = jsonl.read_bytes().decode("utf-8")
    lines = source.split("\n") if source else []
    if lines and lines[-1] == "":
        lines.pop()
    rows = [json.loads(line, object_pairs_hook=exact_object) for line in lines]
    validate_rows(rows, kind)
    if source != "".join(canonical(row) + "\n" for row in rows):
        raise ContractError("canonical_jsonl_required")
    table = pa.Table.from_pylist(rows, schema=schema_for(kind))
    with output.open("xb") as stream:
        pq.write_table(table, stream, version="2.6", compression="zstd", use_compliant_nested_type=True,
                       store_schema=False, write_statistics=False, write_page_checksum=True)
    if output.stat().st_size > MAX_BYTES:
        raise ContractError("bounded_output_exceeded")
    observed = decode(output, kind)
    if observed["rows"] != rows:
        raise ContractError("parquet_roundtrip_mismatch")
    return {"schema": "gptgrep.parquet-write.v1", "status": "written_and_decoded", "rows": len(rows),
            "sha256": sha256_file(output), "bytes": output.stat().st_size}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("runtime", "write", "decode"))
    parser.add_argument("--runtime-receipt")
    parser.add_argument("--input")
    parser.add_argument("--output")
    parser.add_argument("--kind", choices=("document", "single_doc", "cross_doc"))
    args = parser.parse_args()
    try:
        if args.command == "runtime":
            result = runtime_identity()
            if args.output:
                output = file_path(args.output, existing=False)
                with output.open("x", encoding="utf-8") as stream:
                    stream.write(canonical(result) + "\n")
                output.chmod(0o600)
                result = {"schema": "gptgrep.pyarrow-runtime-written.v1", "sha256": sha256_file(output),
                          "pyarrowVersion": result["pyarrowVersion"], "packageFiles": result["packageFiles"]}
        else:
            if not args.runtime_receipt or not args.input or not args.kind:
                raise ContractError("runtime_receipt_input_and_kind_required")
            assert_runtime(args.runtime_receipt)
            source = file_path(args.input)
            if args.command == "decode":
                result = decode(source, args.kind)
            else:
                if not args.output:
                    raise ContractError("exclusive_output_required")
                result = write(source, file_path(args.output, existing=False), args.kind)
        encoded = canonical(result).encode("utf-8")
        if len(encoded) > MAX_BYTES:
            raise ContractError("bounded_output_exceeded")
        sys.stdout.buffer.write(encoded + b"\n")
        return 0
    except Exception as error:
        # Do not echo input rows, metadata values, filenames or runtime secrets.
        code = str(error) if isinstance(error, ContractError) else type(error).__name__
        print(json.dumps({"status": "failed", "code": code}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
