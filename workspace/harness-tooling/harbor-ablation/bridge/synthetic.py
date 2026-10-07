"""Original toy inputs for offline packaging tests, never benchmark data."""
import json
from pathlib import Path
from format_bridge import digest


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def toy_pdf(text):
    escaped = text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
    stream = f"BT /F1 16 Tf 50 750 Td ({escaped}) Tj ET\n".encode()
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>", b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
        f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"endstream", b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    data = b"%PDF-1.4\n"; offsets = [0]
    for i, obj in enumerate(objects, 1):
        offsets.append(len(data)); data += f"{i} 0 obj\n".encode() + obj + b"\nendobj\n"
    start = len(data)
    data += f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode()
    data += b"".join(f"{offset:010d} 00000 n \n".encode() for offset in offsets[1:])
    return data + f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n".encode()


def fixture(root):
    root = Path(root); root.mkdir()
    raw = toy_pdf("The copper vessel holds rainwater.")
    (root / "document.pdf").write_bytes(raw)
    (root / "native.txt").write_text("The copper vessel holds rainwater.\n")
    (root / "tool").write_text("#!/bin/sh\n# synthetic placeholder, never executed by this candidate\nexit 75\n")
    native = (root / "native.txt").read_bytes()
    receipt = {"schema_version":"1", "source_sha256":digest(raw), "artifact_sha256":digest(native),
               "derivation":"synthetic-source-only-v1", "query_agnostic":True, "synthetic":True,
               "preprocessing_latency_ms":0, "preprocessing_cost_usd":None}
    write_json(root / "receipt.json", receipt)
    corpus = {"schema_version":"1", "documents":[{"id":"doc-a", "raw":{"path":"document.pdf","sha256":digest(raw)},
        "prebuilt":[{"asset":{"path":"native.txt","sha256":digest(native)}, "source_sha256":digest(raw),
                     "derivation":receipt["derivation"], "query_agnostic":True,
                     "receipt":{"path":"receipt.json","sha256":digest((root/"receipt.json").read_bytes())}}]}],
        "tool":{"asset":{"path":"tool","sha256":digest((root/"tool").read_bytes())},
                "version":"synthetic-placeholder", "source_revision":"synthetic-no-runtime", "synthetic":True}}
    write_json(root / "corpus.json", corpus)
    questions = ["Which material is named for the vessel?", "What does the vessel hold?"]
    readers = {"schema_version":"1", "corpus_sha256":digest((root/"corpus.json").read_bytes()),
               "tasks":[{"id":f"task-{i}","question":q,"question_sha256":digest(q.encode()),"document_scope":["doc-a"]} for i,q in enumerate(questions,1)]}
    write_json(root / "readers.json", readers)
    options = {"schema_version":"1","reader_image":"example/reader@sha256:"+"a"*64,
        "verifier_image":"example/verifier@sha256:"+"b"*64,"codex_binary_sha256":"c"*64,
        "codex_argv":["codex","exec","--dangerously-bypass-approvals-and-sandbox"],
        "model":"qualification-pending","effort":"qualification-pending","service_tier":"qualification-pending",
        "timeout_sec":60,"cpus":1,"memory_mb":512,"network_mode":"no-network","repetitions":2,"seed":0,"synthetic":True}
    write_json(root / "options.json", options)
    return root


def grader_projection(root, reader_output):
    root = Path(root); reader_output = Path(reader_output)
    readers = json.loads((root/"readers.json").read_text())
    manifest = json.loads((reader_output/"export-manifest.json").read_text())
    value = {"schema_version":"1","corpus_sha256":manifest["corpus_sha256"],
        "reader_projection_sha256":manifest["reader_projection_sha256"],
        "reader_export_sha256":digest((reader_output/"export-manifest.json").read_bytes()),
        "rubric_sha256":"0a8ca971c9779a7fc7c811d1a33ef2e9769527f8a39e5a8b3be2b31f0d49eae8",
        "judge_status":"unavailable","tasks":[{"id":task["id"],"question_sha256":task["question_sha256"],
            "reference_answer":"SYNTHETIC_JUDGE_ONLY_CANARY"} for task in readers["tasks"]]}
    # Canary is an isolation discriminator, never a factual correctness oracle.
    write_json(root/"graders.json", value)
    return root/"graders.json"
