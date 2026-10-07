# GPTgrep Harbor ablation exporter candidate

This offline candidate creates Harbor 0.24.0 tasks from original synthetic inputs.
Its eventual source owner is `workspace/harness-tooling/harbor-ablation/`.
It does not launch Docker, Codex, OCR, Decisions, a judge, or Harbor trials. It does
not install runtimes, read authentication, publish, or upload. Every output is
nonexecuting and requires a new directory; overwrite and resume are refused.
Qualification tags are prospective owner metadata, not a Harbor execution lock.
The candidate has no trial runner or agent adapter. An external Harbor invocation
is outside this source phase and must not use stock agent installation/auth setup.

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
cargo test --offline --locked --manifest-path harness/Cargo.toml
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
