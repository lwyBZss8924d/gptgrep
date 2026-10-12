# GPTgrep Harbor ablation exporter candidate

For the current approved 62-pair SEARCH experiment, start with
[full62/README.md](full62/README.md) and the repository
[development guide](../../../docs/development.md). This directory also retains
earlier exporter, bridge and qualification work; their historical results and
limits below are not the current full62 runtime acceptance.

The offline exporter creates nonexecuting Harbor 0.24.0 tasks through separate
synthetic and source-custodied native-v2 interfaces.
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

Production options default to `network_mode="no-network"` with `allowed_hosts`
absent or an empty list. An explicit `network_mode="allowlist"` requires one to
32 unique canonical exact DNS names or IP addresses in `allowed_hosts`; each
entry is bounded to 253 ASCII bytes. Wildcards, CIDR ranges, URLs, credentials,
ports, paths, IPv6 scope identifiers and noncanonical IP spellings are refused.
DNS fallback also refuses numeric or hex-numeric final labels, including legacy
IPv4 aliases such as `127.1` and `0x7f000001`; names such as `123.api.example.com`
remain valid. Both reader arms receive
the same selected environment and agent policy. The separate verifier retains
explicit `no-network` in both its environment and phase, with no inherited host
list. The synthetic interface retains its offline policy. Config parsing does
not qualify container egress, authentication or a native trial.

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
task phase. The legacy v1 interface continues to refuse nonsynthetic inputs.
The separate production interface below verifies actual generation structure and
file custody; it keeps runtime, helper, and full multimodal qualifications on HOLD.

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
`native={page_window,dpi,render_pages}`, the full parser
`resource_budget`, and `hard_limits={native_wall,native_rss,total_index_publication_bytes}`.
Page window is 1–25, DPI is positive and at most 300, and rendering must be
enabled. There is no whole-document page-count admission limit: every page must
be processed, with PDFium integer representation and actual resource limits
still enforced. The optional legacy `native.max_document_pages` field is
deprecated and ignored, including when supplied in older controls. Its original
bytes remain part of the controls digest; omitting it changes that digest.
Resource selections may only tighten the frozen parser limits. The coordinator uses one shared original ResourceBudget across
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
before publication. Pages requiring qualified OCR remain blocked. Native pages
may retain rendered visual artifacts when `render_pages` is enabled; keeping a
rendition does not establish reader image delivery.

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

The runner, not the control, sets `CODEX_REFRESH_TOKEN_URL_OVERRIDE=http://127.0.0.1:0/oauth/token`, `CODEX_REVOKE_TOKEN_URL_OVERRIDE=http://127.0.0.1:0/oauth/revoke` and loopback `NO_PROXY`/`no_proxy` in the native child's environment; the control's environment allowlist cannot carry or override them. The app-server Judge (`judge_app_server`) and the native Judge backend set the same variables. A native child is a credential consumer: it never refreshes or revokes a shared ChatGPT login, and port 0 can never have a listener that would receive the refresh token. These launchers take no `codex-auth` lease. With a shared login, run them only as consumers under `workspace/harness-tooling/codex-auth` `run`, which owns refresh.

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

The same binary also exposes a fixed app-server broker source preflight:

```sh
gptgrep-harbor-judge-request execute-app-server \
  --control /path/to/verifier/control.json --control-sha256 CONTROL_SHA256
```

It admits the existing `gptgrep.judge.native-control.v1` held inputs and reports
the computed `gptgrep.judge.app-server-recipe.v1` digest separately from the
declared runtime recipe digest. This preflight does not reconstruct the formatter,
open actor homes or claims, initialize a clock or ledger, or launch a process.
Adding `--run` always refuses before these effects because the existing opaque
production permit constructor remains unavailable. A matching caller digest
does not grant execution authority.

The private actor is wired to a bidirectional nonblocking stdio state machine
using the standard `codex app-server --listen stdio:// --strict-config` front door.
It sends one initialize/thread/turn sequence with the original held prompt and
schema and requested `gpt-6-luna`, `max`, `fast`. It binds the returned native
thread and turn IDs, bounds early notifications before binding, and requires an
ordinary `agentMessage` with `final_answer` plus a matching completed turn.
RPC IDs remain distinct from native IDs.
The receipt records at most four outbound frame methods, exact prepared byte
counts and SHA-256 values (including the newline delimiter), plus separately
observed write counts and full-write facts. A prepared frame digest does not
assert that it was written. The actual turn frame includes the returned native
thread ID; its digest is distinct from the request/template recipe digests.
No raw outbound payload is retained in the receipt.

Optional native usage and raw response
IDs stay absent when no matching event is observed; configured acknowledgements
do not establish served identity, cost, or provider request counts. The local
source normalizes a requested Fast tier to `priority`; the broker retains the
raw configured tier and admits `fast`/`priority` as source-equivalent Fast
acknowledgements, while refusing absent, Flex or unknown tiers. This establishes
no served tier or profile. The source map is local protocol evidence, not a claim
about the latest remote version.

Input frames are limited before parsing, with recursive duplicate-key, depth,
node, event-count and aggregate early-buffer limits. Output, final text and
receipt serialization are bounded separately. Only numeric RPC error codes and
fixed failure codes survive rejection; diagnostic, reasoning and stderr content
is not published. Unknown/tool/server requests fail closed, without replies or
retries. Such rejection observes a request or item and does not prove that a
server-owned tool was prevented from running. The actor retains once-only claims
and unknown provider lifetime on failure, with held-source checks before receipt
publication and private files/directories.

Pure protocol and generic Python fake-child tests qualify this source transport
only. Actual byte framing, runtime/profile/tool closure, auth, native identities,
OS resources, shared original-clock accounting, original judge semantics and
signal/cancellation behavior and
Harbor trials remain unqualified. Structural verdicts keep
`judge_status="unavailable"` and emit no reward.

`gptgrep-harbor-judge-request` prepares a verifier-only request or validates a
typed verdict artifact. Its production judge execution policy is unavailable; it performs no model invocation, retry,
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


## Source-custodied production export

### Trusted native-text pilot bootstrap

`gptgrep-harbor-rich-helper` is an explicit host-side retrieval bootstrap for a
controlled pipeline pilot. It calls the existing v2
`prepare_rich_text_search` plan and executes document routing plus evidence
reranking with exactly `openai/gpt-6-luna-decisions`, without retry or fallback.
Successful retrieval requires both calls; failure may stop before either call or
after one. A shared pool can exhaust between stages, so this is not an atomic
two-credit reservation.

Run the helper after sealing a generation built from raw documents and projecting
the current task's query, before the treatment reader starts. Count all helper
execution and query projection time, submissions and observed usage as retrieval.
Its work is never excluded as indexing. The control reader receives raw PDFs.
This initial pipeline is not an interactive agent tool or a utility qualification.

With no arguments the helper emits its nonexecuting frontdoor description.
`--config PATH --config-sha256 SHA` validates bounded inputs and the existing
initialization without joining a ledger, sampling its clock, reading credentials,
writing outputs or submitting requests. Add `--execute` only in the trusted host:
it consumes a new private output directory, joins the already initialized original
ledger, checks the original anchor and expected counters, and reads
`OPENROUTER_API_KEY` from that process's environment. There is no initializer,
endpoint override, reset, resume, deadline renewal or native reader launch.

The closed `gptgrep.harbor.rich_helper.v1` config binds an exact
`GenerationPin`, corpus root, raw UTF-8 query file with bytes/SHA, retrieval
options, expected canonical returned model, output directory and original context.
The context includes ledger path, immutable initialization SHA, original anchor
SHA, run/plan/deadline/submission limit and an exact prior
`ExecutionSnapshot`. Root-owned monetary admission and original clock
initialization remain prerequisites outside this helper.

Only a successful process and completed `accounting.json` authorize passing
`reader-evidence.json` to the treatment reader.
The helper rechecks its retained original context before publishing completed
evidence and after the final accounting sync. Expiration revokes reader evidence
and retains prior observations and charges in failed accounting, without retry.
Evidence preserves the product's source bindings, hit order and bounded coverage without granting citation or
reader-delivery authority. Keep the accounting file and helper credentials outside
both reader environments. Accounting retains original context, observed counters,
checked numeric usage and complete validated-usage digests. Missing values remain
null, failed-stage usage remains unavailable, and observed logical/send-start
counts are never called physical or billed provider requests. Provider freeform
metadata and raw bodies are not retained. No money settlement is performed.
The product's `accounting_complete` flag means every nonzero attempted logical
call returned a validated response; it does not require search success or optional
usage. An observer failure after a validated response can therefore retain true,
while zero-attempt or partially validated failures remain false. The separate
`helper_logical_accounting_complete` requires two validated observations, checked
available token/cost fields, and exactly two reservations, send starts, submission
observations and terminal results in the unchanged original context. Concurrent
unrelated ledger changes invalidate that attribution; failed response usage,
physical requests and billed cost remain outside this completeness claim.

### Rich-batch bootstrap and treatment evidence

The separate `--rich` mode uses the closed
`gptgrep.harbor.rich_batch_helper.v1` config. It binds the raw query plus a
bytes/SHA-pinned `gptgrep.batch.rich.request.v1` file, an exact generation, the
original context, `max_calls`, and an optional `expected_preparation_sha256`.
Only `search_multimodal` and `rerank` operations are admitted, all with the same
query, unique IDs, and `max_concurrent_batches: 1`. Local visual inspection and
complete rendition reads remain available to the treatment reader through the
existing batch tool.

`--rich --prepare-rich --config PATH --config-sha256 SHA` opens the pinned
reader and prepares every intended paid operation. A failed item blocks the
whole helper before submission. It emits bounded envelope metadata and its
preparation digest, with no credentials, logical reservation, provider call or
output directory. The original context is joined and checked without consuming
it. `reserved_calls` is derived from the prepared routing and reranking objects;
it is not fixed at two. Full-pool reranking envelopes are conservative bounds;
actual routing-selected subset bodies and hashes appear in terminal accounting.

Preparation metadata is an integrity binding. The private monetary coordinator
must inspect the actual prepared bodies, reserve the entire bounded phase under
its canonical dollar guard, and approve launch before `--rich --execute`.
The public helper has no monetary-admission authority. Execution requires the
expected preparation digest, recreates that preparation against unchanged input
pins, and retains each actual attempt before deciding evidence delivery. It
requires checked actual model, numeric token/cost observations and exact context
deltas for the derived observed call count. Failures or an expired original
clock prevent reader startup; no retry, refund, deadline reset or money
settlement is performed.

A completed rich helper emits the closed
`gptgrep.harbor.rich_helper_evidence.v2` attachment: source-bound native/visual
references, scores and structured coverage only. It contains no document text,
image bytes, answer, freeform model prose, credentials or accounting. Its
`actual_image_delivery` and `citation_authority` stay false. The treatment must
read cited source bytes and open decoded `read_visual` image bytes through its
native image viewer before claiming visual consumption. Helper preparation,
provider latency, receipt handling and delivery all count as treatment retrieval.

Each reader task may optionally add `helper: {group_id, evidence, accounting}`;
`evidence` and `accounting` use the existing external bytes/SHA input shape.
The exporter validates the attachment against the task question, group,
generation, raw sources, native coordinates or exact rendition, and completed
private accounting. It retains both inputs through publication, copies only the
evidence to the treatment's `/app/helper-evidence.json`, and excludes it from
the control. Rust and Python validate the same closed attachment and namespace.
Source tests exercise native and visual references, stale custody, forged
bindings and baseline exclusion. These sources do not qualify a live reader,
image consumption, monetary admission, Harbor mounts or paired outcomes.

The exporter adds explicit commands with no task selection shortcuts:

```sh
gptgrep-harbor-ablation-exporter plan-production --corpus corpus.json --corpus-sha256 "$CORPUS_SHA256" --readers readers.json --readers-sha256 "$READERS_SHA256" --options options.json --options-sha256 "$OPTIONS_SHA256" --output-dir /path/to/new/plan
gptgrep-harbor-ablation-exporter export-production-readers --corpus corpus.json --corpus-sha256 "$CORPUS_SHA256" --readers readers.json --readers-sha256 "$READERS_SHA256" --options options.json --options-sha256 "$OPTIONS_SHA256" --output-dir /path/to/new/readers
gptgrep-harbor-ablation-exporter export-production-verifier --graders graders.json --reader-export /path/to/readers --output-dir /path/to/new/graders
```

Set `CORPUS_SHA256`, `READERS_SHA256`, and `OPTIONS_SHA256` to the lowercase
SHA-256 digests of the exact bytes in their corresponding input files. Both
commands require all three digests and check them against the held input bytes
before admitting corpus assets.

`validate` dispatches by the exact export schema. Production uses
`gptgrep.harbor.export.v2`; the existing synthetic export remains v1. Python
`format_bridge.py compose` accepts matching reader/grader roles from either
schema, never a cross-schema pair. Production package metadata says
`synthetic=false`, `execution_qualified=false`.

The strict input schemas are `gptgrep.harbor.production-corpus.v1`,
`gptgrep.harbor.production-readers.v1`, `gptgrep.harbor.production-options.v1`, and
`gptgrep.harbor.production-verifiers.v1`. A production corpus has `groups` and a
`tool`. Each group supplies `id`, `source_root`, `runtime_root`, plus
`sealed_receipt`, `sealed_pin`, `sealed_raw_manifest`, and `sealed_controls`.
Every seal input is `{path, sha256}` for an explicit absolute regular file.
The tool supplies `asset_path`, `sha256`, `version`, `source_revision`, `os`, and
`architecture`. These declared tool attributes are bound as metadata; the
exporter never runs the binary or proves its target platform or helper capability.

The corpus admission completes before loading the reader projection. It opens
regular files with no-follow ancestor traversal and retains their descriptors,
checks exact raw/control/seal/current/pin digests, and invokes the public pinned
`RichGenerationReader::verify_all`. It then derives and checks the complete
selected generation inventory, including native text, rendition/metadata/layout
artifacts, prepared PDFs, merged text, and all seven index files. Raw files must
match the manifest's complete group. Unexpected raw or selected-generation files,
symlinks, digest mismatches, duplicate raw inode aliases, and coverage requiring
OCR fail. Historical sibling generations are outside this receipt's scope.
This first production route admits native/verified-blank pages only; it does not
substitute supplied OCR or promote a native-only build to the full harness.

A reader projection binds the exact corpus digest and contains only
`tasks: [{id, question, question_sha256, group_scope, helper?}]`. A scope selects whole
admitted groups. Baseline and treatment receive exactly the same raw files and
Codex options. Production options require the requested reader profile
`gpt-6-luna`/`max`/`fast`, derived from the project role contract. Actual returned
model, runtime, cost, and capability remain unqualified. Options also bind
image and Codex binary digests, direct `codex exec` argv, task time/CPU/memory/
network limits, repeats, seed, and nonzero `reader_uid`/`reader_gid`. A
counterbalanced schedule remains `executed=false`; it does not launch trials.

Production options may additionally supply `runtime_debian_packages: {path,
sha256}` for an absolute, regular manifest file. Omit it or use null to preserve
the existing Dockerfile. The manifest has this strict schema:

```json
{
  "schema_version": "gptgrep.harbor.runtime-debian.v1",
  "packages": [{
    "deb": {"path": "/path/to/runtime/package.deb", "bytes": 1234, "sha256": "<64 lowercase hex>"},
    "license": {"path": "/path/to/runtime/package.copyright", "bytes": 234, "sha256": "<64 lowercase hex>"}
  }]
}
```

The limit is 1–32 packages, 16 MiB per DEB, 256 KiB per license, 32 MiB total,
and 1 MiB for the manifest, within the existing total export ceiling. Admission
checks declared sizes and hashes, regular files through no-follow ancestors, and
the DEB ar signature. Descriptors stay held and all sources are rechecked before
completion. Filenames accept ASCII letters, digits, `._%+:-`; DEBs must end in
`.deb`. Export maps them to fixed `debs/package-NNNN.deb` and
`licenses/package-NNNN.txt` names, preserving original filenames in a frozen
mapping without source paths.

Both reader arms receive identical package, license, mapping and checksum bytes.
The generated fixed Dockerfile instruction verifies checksums and runs offline
`dpkg -i debs/*.deb` before the reader `USER`. It adds no APT lookup, network
download, shell fragment input, or image digest substitution. Installation or
dependency errors fail the build. The pinned `reader_image` must already satisfy
any dependencies outside the supplied DEBs; export itself does not establish
package installability or run a container. Baseline document inputs remain raw
PDFs. Package source/license provenance belongs in the caller-supplied records,
separately from task material; the root-level frozen input manifest binds exact
source metadata, and it is not copied into either arm's image context.
Production validation regenerates the complete reader Dockerfile from the same
options/group/arm constructor used by export, rejecting extra commands or user
changes even when an inventory is refreshed. The Harbor bridge admits the added
root snapshot and per-arm runtime namespace, binds all package/license/mapping/
checksum bytes, and checks the complete Dockerfile before compose. Explicit null
is normalized to the same omitted descriptor field as Rust serialization. Offline
generic fixtures cover enabled, omitted and null options through real exporter,
bridge composition and Harbor `TaskConfig`; these are not container or model runs.

Each task preserves this context topology:

```text
environment/groups/GROUP/raw/ORIGINAL_RELATIVE_PDF_PATH
# treatment only, beneath the same raw root:
environment/groups/GROUP/raw/.gptgrep/CURRENT.json
environment/groups/GROUP/raw/.gptgrep/generations/PIN/
  manifest.json
  text/...
  artifact-text/...
  artifacts/...
  artifact-index/...
environment/groups/GROUP/seal/{receipt,pin,raw-manifest,build-controls}.json
environment/tool/{gptgrep,provenance.json}
```

The reader Dockerfile maps each complete `raw/` tree to its exact
`runtime_root`, with the same requested user ownership in both arms. It does not
rename documents, rewrite manifest roots, flatten index paths, or include a grader
build context. `runtime_root` must equal the canonical root already bound by the
Reader and manifest. An existing macOS-root package is nonexecuting preparation;
Linux portability is a HOLD. Build and seal at the intended canonical Linux path
before admitting a Linux trial. Changing the root or merely refreshing hashes is
refused, and no generated Dockerfile is runtime qualification evidence.

Verifier inputs bind the exact reader-export manifest SHA and require
`judge_status="unavailable"`, a rubric digest, and one
`{id, question_sha256, reference_asset: {path, sha256}}` per reader task. Each
verifier-only reference is the original structured
`{question, answer, answer_format}` with a non-null JSON answer. The reader route
has no reference input. Composition binds the same manifest byte snapshots it
parsed and writes references only in the separate `tests/` context. The original
judge backend and physical denial of grader files to readers remain HOLD; the
unavailable verifier uses a separate `tests/judge-status.json` qualification placeholder, preserves the original `tests/reference.json` unchanged, emits no reward, and retains the answer-equivalence rubric.

Production copies hold source descriptors and use 64 KiB scratch buffers rather
than retaining binary payloads in JSON arrays. The bounds are 64 MiB per file,
128 MiB per complete export/package, 1 MiB per structured input, and a finite
4096-file inventory. Composition reserves two MiB within its package cap for
final dataset/receipt metadata. Source/path identities and inventory names are
checked during admission, before publication, and after copying. Every new
file is `0600` and directory `0700` before content, including under controlled
caller masks `000` and `777`; ancestors are never chmodded. A fresh output is
required. A failure can leave a partial new directory, which is not accepted
without its final inventory manifest and must not be resumed or overwritten.
These ordinary source-custody checks are not hostile filesystem attestation,
hard native RSS/wall enforcement, authenticated builder provenance, or physical
sandbox isolation.

Generic tests build genuine v2 generations through the Writer using explicitly
MOCK parsed DTOs, then replay Reader checks, topology, root refusal, source changes,
reference separation, combined budgets, and typed Harbor composition. They run
no native PDF extraction, agent, provider, Docker, or trial. Full multimodal/tree/
regex/visual/helper delivery, actual Linux/native tools/authentication, original
phase/resource qualification, original judge parity, utility, release, and Goal
completion remain separate gates.


### Native judge runner source foundation

The explicit app-server path additionally admits a root-prepared
`gptgrep.judge.native-control.v2` through:

```sh
gptgrep-harbor-judge-request execute-app-server \
  --control verifier-control-v2.json --control-sha256 CONTROL_SHA256
# A separately adopted finite phase selects the intentional effect frontdoor:
gptgrep-harbor-judge-request execute-app-server \
  --control verifier-control-v2.json --control-sha256 CONTROL_SHA256 --run
```

V2 adds `phase.binding={path,sha256}`. The phase binds the run/invocation/trial/task
and arm, actual prepared manifest/request/source/reference/answer/schema hashes,
original source revision and unchanged formatter, pinned runtime/schema/recipe,
one child/turn, zero retries/fallbacks and a child timeout of at most 120000 ms.
`control_projection_sha256` hashes the serialized control with `phase` omitted;
this avoids a circular phase/control digest. The execution-policy digest equals
the exact phase-file hash. The origin-receipt digest equals the selected origin
file hash. Origin binds the same run, the original serialized `RunClockAnchor`
and its SHA/UTC deadline, plus physical CODEX/SQLite/log directory identities.
Control, phase and origin files are private `0600`; selected native directories
and the permanent claim root are `0700`. No credential or account file is read
by admission.

Preflight samples no clock and opens no native home. `--run` joins the original
anchor, samples its process timer before the original remaining-time check and
keeps both guards through claim, spawn, transport and publication. It never
recaptures or renews a clock. V2 final JSON is bounded to 8192 bytes and its full
worst-case receipt is admitted before claim or native work. Permanent claims and
separate started/finished observations retain uncertain outcomes; interruption
or publication failure grants no retry. Automatic account/rate/disabled-remote
metadata is DTO-validated and discarded under one 32-notification counter across
all phases. Requests for tools, warnings, reroutes and unknown methods remain
refused. The actual private verdict reason stays in `native-final.json`; the
result projection contains only its hash and verdict booleans. Configured profile
acknowledgment remains distinct from unavailable served identity, usage and cost.

The optional Harbor 0.24 custom verifier is selected explicitly as
`gptgrep_harbor_verifier:OriginalNativeJudgeVerifier` with `phase_map` and
`phase_map_sha256` kwargs. Its finite map pins the broker and at most eight exact
trial/task/step entries with control and collected-answer/reference hashes.
It resolves only the successfully collected main-service artifact
`/app/answer.json` with destination `answer.json`, uses the active step's artifact
and evaluator tests paths, and validates them against the prepared phase. It
never takes an agent message as the answer or transfers gold to a reader.
Only a matched, published native completed verdict yields reward 0 or 1 from
`equivalent`; `abstained` is independent. Missing preparation, unknown completion,
changed inputs or unavailable output returns `rewards=None`.

This pilot is explicitly two-phase: collect reader outputs, then root freezes
their exact evaluator request/phase and invokes the native judge. The custom
verifier can replay that already bound evaluator step; it cannot create a live
phase before answers exist. Automatic end-to-end Harbor preparation, real native
judge parity and physical reader/gold isolation remain separate runtime gates.
The default package verifier is unchanged and unavailable. Source/mock checks
do not qualify model semantics, native egress, subscription identity or release.

The verifier-only judge request tool adds a structural preflight front door:

```sh
gptgrep-harbor-judge-request execute-native \
  --control verifier-control.json --control-sha256 CONTROL_SHA256
```

The `gptgrep.judge.native-control.v1` control binds a prepared request root and
manifest, the exact qualified Python executable, a native executable's digest
and known byte length, source/recipe digests, explicit environment and state/log
paths, a once-only invocation ID, a fresh output path, and tighter capture limits.
Phase-origin and execution-policy digests are references; they grant no execution
permission. Preflight holds and rechecks the declared input files and streams the
binary hash. It creates no claim, output, formatter child, native process, home,
clock, or ledger. The explicitly selected control snapshot is read to decode its
fields. Before opening nested request-root, Python, or binary inputs, preflight
rejects their lexical namespace overlap with configured CODEX_HOME, sqlite
and log roots. `environment.home` binds the real OS HOME namespace without
excluding all source/runtime/output paths beneath it. Selected private native
state, claim, output and input namespaces remain separate. Shared TMPDIR is
allowed. It reads only selected control/source
inputs and performs no native config, authentication or state discovery. Private
input, tool, home/account authorization and actual runtime qualification remain
HOLD; this lexical guard establishes none of them and authorizes no home/account
change. This preflight checks structure
and bindings; it does not reconstruct the original formatter or establish judge
parity.

Adding `--run` to legacy v1 or standard exec fails with `native_execution_policy_unavailable` before
any child or durable claim. There is no caller-selected command, permit boolean,
formatter, fallback, retry, or default verifier promotion. Original phase/clock,
tool, home, runtime and response-identity qualification need separate admission.
The generated Harbor verifier remains unavailable and emits no reward.

The legacy exec runner has no production permit constructor. Only test code can admit
self-authored isolated mock children. Under that test permit, it reconstructs the
request with the fixed embedded formatter before consuming an exclusive, durable
claim. It then uses a fixed direct argv recipe, a cleared explicit environment,
nonblocking pipes and one immutable observation deadline. It accepts one complete
bounded thread/turn/final-message/terminal sequence and requires final-file bytes
to equal the stream message, a retained file identity, and the exact two-boolean
and string verdict. File/input identities and digests are rechecked before atomic
publication. Files are `0600`, directories `0700`, and an existing output is never
replaced. A claim is never deleted or refunded; failure, deadline, cleanup or
publication uncertainty cannot authorize another attempt. A late detected source,
control, binary or claim-root custody failure after receipt serialization clears
the typed verdict, marks the observation unavailable/quarantined, and publishes
no output. Already observed transport IDs, projected usage and raw-final hash
remain scoped observations, with the consumed claim retained and no retry.
Publication status is separate from execution observations; a post-rename sync failure can leave output
and remains unknown with no retry.

Control bytes are at most 64 KiB. The binary is admitted by known length at most
1 GiB and hashed through a 4 KiB scratch buffer. Capture limits are at most 1 MiB
stdout, 64 KiB stderr, 64 KiB per event, 4096 events, 64 KiB final bytes and 64 KiB
receipt. The scalar-only failure receipt allowance is checked before effects;
a typed reason that does not fit clears the verdict and preserves a bounded
unqualified failure observation. Raw stderr is counted and hashed, never retained.
Caller limits may tighten these bounds only if the failure receipt still fits.
The original source formatter retains its dynamically read response-character cap
of at most 12000, Python normalization and truncation semantics.

Requested `gpt-6-luna`/`max`/`fast` stays distinct from observed identity. Standard
exec JSON exposes no native turn ID or served profile, and its projected numeric
usage can default to zeros when usage was absent. Native turn/model/provider/
effort/tier/response identity, actual usage/cost/provider calls therefore remain
null and usage presence remains unknown. Mock thread/item IDs are labelled as
transport fixture observations. Even a well-formed complete sequence and typed
verdict is structural and unqualified, with no numeric reward.

The source-supported recipe does not prove tools are disabled, user state cannot
be written, flags are live-supported, or the requested profile was served.
macOS mock startup added the platform environment key `__CF_USER_TEXT_ENCODING`;
no value or inherited provider credential was read. Named owned-buffer/capture
limits and bounded leader cleanup do not prove total RSS, aggregate disk/inode
reservation, hostile path-to-exec custody, descendant/remote lifetime termination,
or a hard OS deadline. These limits, actual native/backend/original-judge semantic
qualification, Linux/Harbor trials, utility, release and full acceptance remain
HOLD.
