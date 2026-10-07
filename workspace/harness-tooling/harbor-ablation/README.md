# GPTgrep Harbor ablation exporter candidate

The offline exporter creates Harbor 0.24.0 tasks from original synthetic inputs.
The source owner is `workspace/harness-tooling/harbor-ablation/`. Its export and
format-composition commands create nonexecuting packages in new directories;
overwrite and resume are refused. These commands launch no Docker, Codex, OCR,
Decisions, judge or Harbor trials, and perform no runtime installation,
authentication reads, publication or upload.

An optional custom native agent adapter has a separate explicit execution
frontdoor, described below. Qualification tags are prospective owner metadata,
not a Harbor execution lock. Actual trial admission requires independently
qualified sandbox assets and a frozen phase; stock agent installation/auth setup
is outside this source contract.

The primary comparison is the whole prebuilt search harness. The control reader
gets raw PDFs and the same Codex configuration as the treatment. It may freely
parse the PDFs with native tools in its eventual owned sandbox. The treatment
also gets source-bound prebuilt artifacts and GPTgrep. Source preparation is
excluded from the answer-equivalence reward and measured separately. The
treatment's future Decisions helper and OCR cost belong in the resource report;
baseline parsing time and cost belong in its trial runtime. No retrieval-only
causal claim follows from this comparison.

## Interfaces

The Rust standalone package implements:

* `plan --corpus ... --readers ... --options ... --output-dir ...`
* `export-readers` with the same arguments
* `export-verifier --graders ... --reader-export ... --output-dir ...`
* `validate --export-dir ...`

Planning and reader export accept optional `--limit N` and comma-separated
`--task-ids`. Unknown or duplicate IDs and zero limits fail. Selection follows
the frozen reader projection order, never answer locations. The grader projection
must contain exactly the selected IDs and question digests. A paired schedule
contains every task, repeat, and arm; first-arm order reverses between repeats and
alternates across task ordinals. Schedule rows remain `executed=false`.

`Corpus` is an immutable, exact-file-digest-bound source manifest. It admits only
explicit relative regular files through bounded no-follow reads. Each prebuilt
artifact has a raw source digest and a separate query-agnostic derivation receipt.
`Readers` contains only questions and document scopes. Its typed schema rejects
reference answers and other unknown fields. `Graders` is a separate front door
containing reference answers; it binds the exact reader-export manifest digest.
The Rust reader command has no grader input.
The grader export retains its expected exact reader-export manifest SHA-256;
composition compares that digest to the same immutable manifest buffer it parsed,
before creating output. Its receipt retains the admitted reader and grader buffer
digests; it does not reopen their paths for identity. Matching corpus/query/
options/task tuples alone cannot authorize another reader export. Reader and
plan manifests require a null expected-export field; grader manifests require
one valid digest. These are caller-provided hash bindings, not public signatures.

The thin UV-managed Python project pins `harbor==0.24.0`. `format_bridge.py compose`
takes the two admitted export directories and writes a new Harbor package. It
uses Harbor's TaskConfig 1.4, DatasetManifest 1.0 and native
`Packager.compute_content_hash`; the deprecated `Task.checksum` is not used.
`format_bridge.py validate` rechecks the exact inventory, task/dataset schemas,
native digests, separate verifier context, and answer artifact source.
All composed files use the same create-new, no-follow private writer with explicit
mode `0600`. Every new directory component uses `0700`; existing caller parents
are never chmodded. Controlled subprocess tests cover caller masks `000` and
`777` without changing the test parent's or global umask.

The task tree is:

```text
control/ and gptgrep/
  dataset.toml
  task-ID/
    task.toml
    instruction.md
    environment/              # reader Docker build context
      Dockerfile
      raw/*.pdf
      reader-input.json
      codex-config.json
      prebuilt/ and tool/     # treatment only
    tests/                    # separate verifier Docker build context
      Dockerfile
      test.sh
      verify_answer.py
      reference.json
paired-schedule.json
package-receipt.json
format-status.json
```

The reader Dockerfile copies only its own context. The explicit verifier
`environment_mode="separate"` has its own Dockerfile and cannot fall back to the
reader image. Harbor collects `/app/answer.json` with `destination="answer.json"`;
the verifier reads the original `/app/answer.json` source path after restoration.
The grader-only canary is a role-isolation discriminator, never a correctness
oracle. Runtime mount/read-denial is unqualified until a sandbox test proves it.

## Offline checks

Use an owned canonical, non-symlink scratch directory. Cargo dependencies are
exactly pinned and locked against the existing project identity seed:

```sh
GPTGREP_MOCK_PYTHON=/path/to/qualified/harbor-python cargo test --offline --locked --manifest-path harness/Cargo.toml -- --test-threads=1
cargo fmt --manifest-path harness/Cargo.toml -- --check
cargo clippy --offline --locked --manifest-path harness/Cargo.toml --all-targets -- -D warnings
cargo build --offline --locked --manifest-path harness/Cargo.toml
```

Run Python with a previously qualified Harbor 0.24.0 interpreter, without syncing
or installing anything. The private receipt binds the interpreter and imported
contract files used for the checks. A portable example is:

```sh
uv run --offline --no-project --no-sync --no-python-downloads --python /path/to/qualified/harbor-python /path/to/qualified/harbor-python -B bridge/test_bridge.py --exporter /path/to/exporter --scratch /path/to/owned/scratch
uv run --offline --no-project --no-sync --no-python-downloads --python /path/to/qualified/harbor-python /path/to/qualified/harbor-python -B bridge/demo.py --exporter /path/to/exporter --output-dir /path/to/new/owned/output
```

The first offline `bridge/uv.lock` resolution failed because Harbor's registry
distribution was absent from the UV cache. That failure is retained. A separately
authorized single public PyPI metadata resolution then produced a lock for exact
Harbor 0.24.0, with no configuration discovery, private index or credential
inheritance, installation, or Python download. All 92 registry package entries
use the explicit public index. The eight selected installed contract dependency
versions match the lock. Locked archive sources have not been installed or
compared to the observed contract files. The reconstructed locked environment
remains a HOLD; tests in the existing environment do not qualify another runtime.

## Qualification boundaries

The synthetic tool exits unavailable and the image/model/binary pins are explicit
fixtures. They are not GPTgrep or Linux Codex capability evidence. The exporter
checks source/projection/digest structure; it does not semantically prove that an
arbitrary claimed derivation is a genuine GPTgrep tree/tgrep build. The production
preprocessor must produce and independently qualify those receipts before a real
task phase. This initial candidate deliberately refuses nonsynthetic inputs.

The verifier admits only a bounded `{"answer":"..."}` artifact. It preserves the
original answer-equivalence-only rubric boundary and 12000-character answer cap,
but no original judge adapter has yet been qualified. It writes an unavailable
status and exits 75 without writing a numeric reward. No citation, parsing, or
preprocessing bonus exists. Synthetic correct/incorrect verdict mapping tests do
not establish rubric parity or answer quality.

Remaining gates include locked Python environment reconstruction and source identity, actual source-only tree/tgrep
and multimodal delivery, latest Linux Codex artifact/config qualification, equal
native tools/resources/network policies, trusted sandbox authentication, physical
grader denial, original judge parity, answer artifact restoration, and a concrete
frozen trial phase. No oracle solution, PageIndex content conversion, licensing or
redistribution acceptance, Harbor A/B advantage, default promotion, M7 completion,
or experimental release follows from this candidate. SPEC's live PageIndex first
release comparison gates remain in force. The earlier plans are immutable; the
later user budget authority is recorded separately in the private amendment.


## Raw-only prebuild source foundation

The dedicated `gptgrep-harbor-prebuild` binary has two explicit frontdoors:

```sh
cargo build --offline --locked --manifest-path harness/Cargo.toml --bin gptgrep-harbor-prebuild
gptgrep-harbor-prebuild build-native --root /app/raw/group-ID --raw-manifest /app/control/raw.json --build-controls /app/control/native.json --output-dir /app/seals/new-build
gptgrep-harbor-prebuild seal --root /app/raw/group-ID --pin /app/control/pin.json --raw-manifest /app/control/raw.json --build-controls /app/control/native.json --output-dir /app/seals/new-seal
```

These operations accept no reader, grader, question, answer, evidence annotation,
model, account or runtime-launch argument. The raw manifest is strict JSON with
`schema_version="gptgrep.harbor.raw-group.v1"`, one `group_id`, and a sorted unique
`documents` list of `{path,bytes,sha256}`. It admits at most eight explicitly listed
PDFs, unchanged relative names and at most 64 MiB aggregate raw bytes. The corpus
must contain exactly that raw set; `.gptgrep` is the reserved product state.
Manifest, controls, pin, preparation staging and fresh output live outside the
corpus. Input leaf/ancestor symlinks, traversal, control overlap and extra files
are refused. Metadata/digests remain bound to the admitted buffers and held file
descriptors; replacing an input path cannot change its admitted identity.

Controls use `schema_version="gptgrep.harbor.native-controls.v1"`, the same
`group_id`, `observation_wall_seconds` (1–180), `raw_group_bytes` (at most 64 MiB),
`seal_inventory_bytes` (at most 1 GiB), `seal_inventory_files` (at most 4096),
`native={page_window,dpi,render_pages,max_document_pages}`, the full parser
`resource_budget`, and `hard_limits={native_wall,native_rss,total_index_publication_bytes}`.
Page window and document page count are 1–25, DPI is positive and at most 300,
and rendering must be enabled. Resource selections may only tighten the frozen
parser limits. The coordinator uses one shared original ResourceBudget across
preparation, extraction, finalization and writer import; counters are not reset
between documents. Every hard-limit switch must be false. Unsupported requested
hard guarantees fail before writer creation.

`shared_staging_reservation_bytes_observed` is the shared invocation's conservative
reservation counter, not measured physical disk usage. Temporary preparation
directories are removed on successful import and on errors; the counter does
not refund their reservations. An unpublished writer generation may remain for
inspection after failure. Historical reservation usage is unknown for `seal`.

`build-native` invokes the genuine parser preparation and immutable native
candidate APIs. It inspects the candidate plan before finalizing with no outcomes
and `OcrMode::Off`. Required OCR, OCR tasks or capability blockers refuse import
and publication. It never creates successful OCR receipts or rewrites native
coverage. Raw source checks run before/after native awaits, before import and
before publication. This native-only round intentionally blocks visual/scanned
sources requiring qualified OCR.

The observation deadline is checked at admission and operation boundaries.
PDFium extraction is a blocking library call, and synchronous writer publication
builds the index before returning. The coordinator cannot enforce hard native
CPU/RSS/wall limits, drain all library diagnostics, or prevent publication after
a deadline that expires inside that call. A post-publication deadline or seal
failure may leave the new CURRENT without a seal receipt. Parser/raw/shared
staging failures before publication preserve the previous CURRENT. The seal byte
limit admits the selected manifest before reader deserialization and bounds
inventory verification; receipt JSON has its own limit. It is not a hard
index-publication limit. Linux executor/cgroup/disk/process cleanup and bounded
native diagnostics remain separate qualification requirements.

`seal` requires the same admitted CURRENT bytes to name the exact supplied pin.
This seal frontdoor is native-only: every page must be Native or BlankVerified,
and historical OCR task/result arrays or ValidatedOcr coverage are refused.
It uses `RichGenerationReader::verify_all`, compares every manifest document with
the exact raw set, and verifies the complete selected generation file set:
prepared/native/layout/tree/rendition files, artifact text and all seven pinned
index files. The index engine revision, file-table identity and manifest digest
come from the validated admitted index buffer. All retained input/source/artifact
snapshots, the selected file set and the reader are checked again before output.
A CURRENT change refuses the seal even if its retained generation remains valid.
Unused sibling generations and write-lock history are explicitly unverified.
Moving a generation to a different canonical root is refused; no root rewriting
or import/relocation authority exists.

A successful seal atomically creates a new directory containing `receipt.json`,
`pin.json`, the same admitted raw manifest and build-control bytes. Existing
outputs are never overwritten. Verification, collision, byte and observation-deadline
failures before rename produce no fresh receipt. A parent-directory sync failure
after successful rename leaves publication durability unknown and reports
`publication_sync_unknown_no_retry`; the completed output may exist for owner
inspection. The stored receipt records verified structure before publication;
owner exit/status evidence separately records observed durability. Linux/macOS have a no-replace directory primitive;
other platforms fail closed and remain unqualified. Receipt JSON is at most
1 MiB. The receipt records exact raw/control/CURRENT/pin/artifact/index/coverage
bindings, selected resource limits and available observations. Unknown builder
binary/source identity, PDFium artifact, peak RSS and hard-executor qualification
stay explicit. `current_frontdoor_provider_calls=0` describes this coordinator
or seal invocation only. Historical producer/OCR request counts, usage and cost
and historical native extraction counts/cost are unknown. Native DPI/render settings govern `build-native`; `seal` records
the requested controls and structurally bound historical native profile without
attesting that historical extraction executed those settings. The wrapper retains raw descriptors and digests, with zero retained
raw payload bytes. Its admitted JSON-byte buffers have an independent 8 MiB
content ceiling, and generation descriptors/paths have a bounded file count.
These are separate from parser-owned working admission and exclude allocator
capacity, typed library graphs, native buffers and global RSS. A structural seal does not authenticate historical producer
execution or qualify native/OCR correctness. Mock fixtures are labeled MOCK and
use synthetic DTOs with the genuine writer/reader; they make no native PDFium or
provider calls. Source freshness is checked at operation boundaries, with sealed
owners required; no hostile concurrent in-place mutation guarantee is asserted.

The production exporter's synthetic-only gate remains in force. A genuine
raw-only Linux builder execution with qualified binaries/native library, full
required OCR coverage, actual resource/cost observations, independent receipt
adoption and whole-group exporter integration is still required. These source
frontdoors do not qualify D3/D4/D7, native tools, trials, judge parity, advantage,
default promotion, release or Goal completion.


## Optional native Harbor agent source

The optional custom Harbor binding is `gptgrep_harbor_agent:FrozenCodexAgent` (Harbor 0.24.0). It subclasses BaseAgent, uses preinstalled assets and performs no installation, host auth/config copying, model-connection lookup or environment fallback. It accepts explicit `model_name="gpt-6-luna"` and strict options `launcher_path`, `control_path`, `control_sha256`, `user` (nonroot) and `transport_timeout_sec`. Unsupported Skills, MCP, configuration, resume/load and extra environment controls are rejected.

The Rust entrypoint is `gptgrep-codex-native-agent --control <absolute-sandbox-path> --control-sha256 <sha256>`. Default mode admits inputs only; explicit `--run` launches one fresh standard `codex exec` child. The prompt is passed as exact stdin bytes, never interpolated into a shell command. Frozen controls select the SPEC reader profile gpt-6-luna/max/fast, raw PDF inventory, answer schema, absolute owner-supplied wall and monotonic phase deadlines with origin/clock namespace references, output namespace, exact binary/assets and an explicit nonsecret environment allowlist. The owner must derive/qualify those original deadlines before effects. This launcher does not join Jev RunClock and does not prove boot/namespace equality.

Baseline input is raw PDFs only. Treatment must bind genuine v2 generation, regex/tree, visual, multimodal and Decisions-helper capability receipts; these references do not themselves establish qualification. A native-only prebuild scaffold is not the complete treatment. Both arms require independently qualified identical runtime/model/config/native tools/resources/network policy. YOLO flags act inside the external Harbor sandbox; ignore-config flags and scoped home paths are not proof of physical isolation or native entitlement.

The only answer is the real `<cwd>/answer.json` (`/app/answer.json` in task images): an object containing exactly one string `answer`, at most 12,000 characters and 65,536 encoded bytes. Native final-message output does not replace a missing answer. The adapter does not grade or emit reward. It captures bounded event-kind/hashed-item/numeric-usage projections and discards raw stderr/tool text; ATIF support remains false. Requested profile is distinct from actual model/provider/effort/tier/provider-request count/cost, which remain unavailable from the basic native JSONL schema. Missing usage stays null; model attribution is not inferred from the requested name.

Current validation is offline synthetic Rust children and FakeEnvironment fixtures only. Actual latest Linux binary/CLI/auth/profile capability, read-only mounts, tools, physical grader exclusion, namespace equivalence, descendant reaping, hard RSS/network/resource controls, Python lock reconstruction, production treatment provenance and paired trials remain unqualified. The new sources add no automatic trial execution or default/model promotion; all SPEC release gates remain unchanged.

Cancellation and original phase validity are checked before ownership claims and immediately before spawning. All fixed output names, including the pending receipt, are admitted and rechecked before launch. These checks do not claim hard atomic OS cancellation or filesystem isolation. Terminal usage is validated and summed in a local candidate and committed together with terminal state only after the full event and projection are valid; a malformed or overflowing later field preserves prior completed observations.

The native Rust fixtures require the explicit `GPTGREP_MOCK_PYTHON` interpreter
above and run serially because they exercise shared cancellation state. They do
not execute the native main signal-handler path or qualify parallel cancellation
and real Linux cleanup. The separate Python fixtures use FakeEnvironment only:

```sh
uv run --offline --no-project --no-sync --no-python-downloads --python /path/to/qualified/harbor-python /path/to/qualified/harbor-python -B bridge/test_native_agent.py
```

## Original judge request foundation

`gptgrep-harbor-judge-request` prepares a verifier-only request or validates a
typed verdict artifact. It has no judge backend, model invocation, retry,
fallback or reward output. The generated Harbor verifier remains unavailable.
Reference answers, questions, expected formats and real answer artifacts belong
only to the verifier. Source documents, indexes and citations are not inputs.

```sh
gptgrep-harbor-judge-request prepare \
  --judge-source /path/to/pinned/eval/judge.py --judge-source-sha256 SOURCE_SHA256 \
  --original-revision ORIGINAL_REVISION \
  --reference /path/to/verifier/reference.json --reference-sha256 REFERENCE_SHA256 \
  --answer /path/to/real/answer.json --answer-sha256 ANSWER_SHA256 \
  --python /path/to/qualified/python --python-sha256 PYTHON_SHA256 \
  --output-dir /path/to/fresh/verifier/request

gptgrep-harbor-judge-request validate-verdict \
  --request-root /path/to/verifier/request --request-manifest-sha256 MANIFEST_SHA256 \
  --verdict /path/to/verifier/verdict.json --verdict-sha256 VERDICT_SHA256 \
  --python /path/to/qualified/python --python-sha256 PYTHON_SHA256 \
  --output-dir /path/to/fresh/verifier/validation
```

The strict reference schema is `gptgrep.judge.reference.v1` with `question`
(string), `answer` (non-null JSON value) and `answer_format` (string). The real
answer artifact has exactly `{"answer": "..."}`. Missing, malformed, empty or
oversized artifacts refuse preparation; they never become an invented
abstention or an incorrect answer. This separate reference schema does not
upgrade existing Harbor reference packages or their unavailable verifier.

The fixed formatter is embedded in the Rust binary. Only explicitly selected,
hash-admitted Python runs, with direct `-I -S -c` arguments, a cleared
environment and a private temporary cwd. No shell or caller-selected formatter
code exists. Admitted source/reference/answer bytes travel over framed stdin.
The upstream source is inert: the formatter parses AST literals for `PROMPT`,
`SCHEMA`, `MAX_RESPONSE_CHARS`, `MODEL` and `EFFORT` without importing or executing
that module. Requests use Python's exact split-space question normalization,
value string formatting and character truncation. The entire real answer
snapshot remains bound even when its response is truncated for the request.
Original rubric values or task examples are not stored in reader product code.

Source and Python executable files are at most 1 MiB each; reference, answer and
verdict artifacts are at most 64 KiB. Framed stdin is bounded by those three
input limits plus 24 header bytes, formatter stdout by 256 KiB and stderr by
16 KiB. Request JSON is at most 256 KiB; manifests and validation JSON are at
most 64 KiB. The original response limit is dynamically extracted and must be
1–12000 characters. Supported original verdict schema is an exact object with
required boolean `equivalent`, boolean `abstained`, string `reason`, and no
additional properties. A reason is admitted up to 16 KiB. Input content bounds
exclude allocator capacity, AST/library graphs, Python loader/stdlib/native
memory and global RSS. The Python identity pin is the selected executable;
whole-interpreter/runtime reconstruction is a separate qualification.

The formatter has a ten-second observation deadline. On observed expiry or
output overflow, the coordinator attempts termination, observes for another
second, and reports unavailable or cleanup unknown. No hard OS/native wall,
RSS, descendant cleanup or hostile concurrent file-mutation guarantee is
claimed. Unknown cleanup grants no retry. Artifact owners must keep admitted
inputs and the selected Python executable sealed; held descriptors and
before/after digest/identity checks detect ordinary changes. Upstream modules,
site/user modules, authentication homes and provider configuration are not
loaded. This pure formatter child is distinct from Codex/native model or
provider execution and never creates a RunClock or ledger.

Preparation atomically publishes fresh files with mode 0600 in a directory with
mode 0700: request, binding manifest and the same source/reference/answer byte
snapshots. Validation reconstructs formatting with the same embedded formatter
before admitting a verdict envelope bound to exact request/source/reference/
answer/schema digests. It retains all upstream boolean combinations, including
an abstention that is equivalent to the reference. Output remains structural:
`judge_status="unavailable"`, actual judge/profile/provider/usage/cost unknown,
and `reward_emitted=false`. It creates no `reward.txt` or `reward.json` and
cannot authenticate a semantic/native judge result. Requested future profile
is `gpt-6-luna`/`max`/`fast`; this is separate from actual null identity fields.

Outputs require a fresh Linux/macOS namespace outside admitted inputs and are
never overwritten. Failures before rename leave no fresh output; after-rename
parent sync failure reports publication durability unknown, output may exist,
and no retry is authorized. Original semantic/rubric parity, actual transport,
cost, native profile, physical verifier gold isolation, Harbor/Linux execution,
trials, advantage, release and Goal completion remain HOLD.


The native coordinator imports artifacts from the prepared document's exact
retained staging root. Its enclosing temporary directory stays alive throughout
import. Preparation and seal temporary directories use explicit `0700`
permissions, and seal files use explicit `0600` permissions before content is
written. These permissions do not depend on the caller's umask. Source fixtures
exercise genuine identity preparation and mock finalized imports; they do not
qualify native PDF extraction, Linux cleanup, physical resource limits, or
production derivation. Controlled umask fixtures cover the coordinator helpers
and seal only; the unchanged parser and Writer may still fail under a restrictive
caller umask before these helpers can complete. Native execution across arbitrary
umasks remains unqualified. The parser retains its frozen admission budgets.
