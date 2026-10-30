# Harbor integration and XL verification

`native_agent.py` is the pinned native-reader bridge. Its optional project-owned
environment, `proc_environment.py`, is selected through Harbor's supported
`environment.import_path` field as
`proc_environment:ProcOverlayDockerEnvironment`. The controller and wrapper
require the exact selected module/contract pins and the existing owner's source,
mount, security and resource declarations before launch. The bridge accepts
that exact class and joins its created full container ID to the owner's
namespace plan; it does not accept arbitrary Docker subclasses.

The environment replaces only the necessary container creation and ownership
operations. It inherits Harbor's orchestration and Unix task APIs, verifies the
three framework-generated log binds before suppressing them, and refuses extra
user mounts. The reviewed request preserves the two `/sys` masks and clears the
proc masks/readonly overlays needed by the selected native sandbox. Its one-use
Engine path never retries an ambiguous create/start or adopts a container by
name. Exact-ID stop/removal cannot prune images, volumes, networks or orphans.
An actual owner terminal receipt remains necessary after Harbor stop returns.

`test_proc_environment.py` contains isolated transport fixtures. Its additional
installed-Harbor tests require both `GPTGREP_HARBOR_PROC_TEST_PINS` and
`GPTGREP_HARBOR_PROC_TEST_PINS_SHA256`, naming an exact source manifest supplied by
the checked test worker. That layer exercises the real pinned Harbor methods
with process/Engine edges blocked or simulated; it establishes source
compatibility only. Neither test layer supplies native reader, E4 or live Harbor
acceptance.

This directory contains a small Python interface around the exact upstream native
scorer and RewardKit. Python dependencies are declared in `pyproject.toml` and
managed with uv. Acquisition, OCR and model execution do not occur here.

Prepare dependencies only in the admitted execution environment:

```sh
uv sync --project benchmark/harbor
```

The controller supplies separate paths for frozen answer, trusted expected
bindings, verifier-only gold and independently produced judgment:

```sh
uv run --project benchmark/harbor --no-sync --offline python benchmark/harbor/verify.py \
  --answer "$RUN_ROOT/frozen/answer.json" \
  --expected "$RUN_ROOT/verifier/expected.json" \
  --gold "$RUN_ROOT/verifier/gold.jsonl" \
  --native-scorer "$RELEASE_ROOT/code/evaluate.py" \
  --judgment "$RUN_ROOT/controller/judgment.json" \
  --output-dir "$RUN_ROOT/verification" --rewardkit
```

`expected.json` is controller-owned:

```json
{
  "schema": "gptgrep.verifier-binding.v1",
  "task_id": "<enrolled source ID>",
  "reader_status": "completed",
  "answer_sha256": "<SHA-256 of complete frozen answer.json bytes>",
  "corpus_sha256": "<corpusDigest of document IDs and raw SHA-256 values>",
  "rubric_sha256": "<SHA-256 of the executed judge rubric>"
}
```

Supply the actual controller-observed `reader_status`; a failed or timed-out
attempt remains a native zero even if it left a parseable partial answer file.

The judgment uses schema `gptgrep.grounding-judgment.v1`, the same ID and three
hashes, `status: complete|unknown`, `grounded_success: 0|1|null`,
`atomic_coverage: number|null`, and an optional reason. Unknown is never an
invented numeric zero. The complete frozen answer should be a JSON object with a
string `answer`, a `citations` array of `{document_id, page}`, and optional
`explanation`; PDF page indices start at one.

Outputs are `feedback.json`, `native-metrics.json`, `reward.json`, RewardKit
criterion details, and generated criterion files. They remain local run evidence.
`reward.json` has independent native and grounding keys, with no aggregate reward.
Read `feedback.json` for unknown/error status and data completeness before any
promotion decision. The generated Harbor `test.sh` cannot acquire a controller
judgment and therefore reports grounding unknown until that judgment is supplied
through the independent controller invocation.
