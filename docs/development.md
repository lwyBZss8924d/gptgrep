# Development and evaluation setup

This guide covers the experimental source in
[`lwyBZss8924d/gptgrep`](https://github.com/lwyBZss8924d/gptgrep), including the
native parse/OCR path and the full62 Harbor adapter. Read the
[current status](experimental-status.md) for measured outcomes and open gaps.
[SPEC](../SPEC.md) owns acceptance; these instructions do not change its gates.

## Checkout and storage

Set `GPTGREP_BRANCH` to the published experimental branch you are reviewing, such as
`claude/exp-ablation-opt-loop-20261013` or a subsequently published integration
branch. Each branch's
dated [status](experimental-status.md) describes the source that contains it; the
earlier `codex/gptgrep-unified-runtime-20261012` branch predates the shared
Decisions budget commands, the credential supervisor and alias admission
documented here.

```sh
git clone --branch "$GPTGREP_BRANCH" https://github.com/lwyBZss8924d/gptgrep.git
cd gptgrep
git rev-parse HEAD
```

Record that revision for every build and result. Keep private controls and results
under an ignored directory or an explicitly owned external directory. The
maintained workspace follows [development storage](../workspace/harness-config/development-storage.md).
Another workstation must provide its own storage admission and physical paths;
the private registry is not part of this source distribution.

The following variables describe output locations, not storage authorization.
Set `GPTGREP_DEV_ROOT` to a real, owned, adequately sized build volume before
running download/build/test commands. Create the selected directories there.

```sh
export GPTGREP_DEV_ROOT=/absolute/owned/external/gptgrep-dev
export CARGO_HOME="$GPTGREP_DEV_ROOT/cargo-home"
export CARGO_TARGET_DIR="$GPTGREP_DEV_ROOT/cargo-target"
export UV_CACHE_DIR="$GPTGREP_DEV_ROOT/uv-cache"
export UV_PYTHON_INSTALL_DIR="$GPTGREP_DEV_ROOT/uv-python"
export TMPDIR="$GPTGREP_DEV_ROOT/tmp"
mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR" "$UV_CACHE_DIR" \
  "$UV_PYTHON_INSTALL_DIR" "$TMPDIR" "$GPTGREP_DEV_ROOT/results"
```

Container images, layer storage and job outputs need their own external capacity;
`TMPDIR` does not relocate Docker's data root. Preserve normal proxy/CA and OS
gateway policy. Local IPC and provider egress are separate readiness checks.

## Dependencies and local build

| Component | Selected requirement |
| --- | --- |
| GPTgrep | Rust/Cargo with edition 2024; CI selects Rust 1.96.0, rustfmt and Clippy; use `Cargo.lock` |
| Native parsing | Pinned LiteParse Rust source and PDFium `chromium/8058`; an existing LibreOffice installation for Office conversion |
| Codex login and ordinary reader | Caller-selected official Codex CLI; the Harbor common base pins 0.162.1 |
| Native Document/OCR runtime | Caller-selected pinned standalone app-server; retained fixed-image profiles use the derivative described below |
| Python tools | uv-managed Python 3.12+; the common evaluation image uses Python 3.13 |
| Raw-document evaluation tools | Python, Pillow 12.3.0, Poppler `pdftotext`/`pdftoppm`, shell, and matching Codex/Code Mode host binaries in both arms |
| Harbor | Exact `harbor==0.24.0` from the adapter's `uv.lock`, plus an independently managed Docker/Buildx environment |
| Optional wrapper | Node 24 and pnpm 11.24.0, using `packages/cli/pnpm-lock.yaml` |

```sh
cargo build --release --locked -p gptgrep-cli
export GPTGREP_BIN="$CARGO_TARGET_DIR/release/gptgrep"
"$GPTGREP_BIN" doctor --json
"$GPTGREP_BIN" --schema
"$GPTGREP_BIN" --llms
"$GPTGREP_BIN" index ./evals/fixtures/corpus --json
"$GPTGREP_BIN" search 'retention|expiry' ./evals/fixtures/corpus --mode regex --json
```

Plaintext works without loading PDFium. The first native build may download the
pinned PDFium payload into the selected build/cache area. An explicitly provided
`PDFIUM_LIB_PATH` selects its library directory; `PDFIUM_INCLUDE_PATH` selects
headers for the build helper. For distribution, use the complete launcher,
library and license bundle from [release packaging](release.md), not a copied
standalone `gptgrep.bin`. Python/Pillow/Poppler and Codex remain separate agent
runtime dependencies rather than dependencies of a local regex command.

## Independent ChatGPT authentication

Use an independent existing account home for this project's Dev/Test/Eval roles.
Authenticate with the official CLI; the standalone app-server derivative is not
a replacement login command. Set a caller-owned path and let Codex manage its
credential store:

```sh
export GPTGREP_CODEX_HOME=/absolute/private/gptgrep-codex-home
mkdir -p "$GPTGREP_CODEX_HOME"
chmod 700 "$GPTGREP_CODEX_HOME"
CODEX_HOME="$GPTGREP_CODEX_HOME" codex \
  -c 'cli_auth_credentials_store="file"' login --device-auth
CODEX_HOME="$GPTGREP_CODEX_HOME" codex \
  -c 'cli_auth_credentials_store="file"' login status
```

Complete the one-time code in the browser when prompted. Device login must be
enabled for that ChatGPT account/workspace. `login status` checks authentication
without printing credentials; model availability and an actual permitted turn
remain separate checks. See [official Codex authentication](https://learn.chatgpt.com/docs/auth).
Never put the credential cache in a build context or source commit. Harbor mounts
the selected cached file read-only at runtime. Trials never refresh it: their
native children point the token and revoke endpoints at loopback port 0, where
no process can listen.

When several roles share one login, run every consumer through
[`workspace/harness-tooling/codex-auth`](../workspace/harness-tooling/codex-auth/README.md).
It provides one lease per login, a lease guardian, and a per-run `CODEX_HOME`
that links to the single `auth.json` only while the run holds the lease. It also
provides an allowlisted environment that blocks consumer refresh and revoke, one
refresh owner, and a re-login that never touches the existing login until a new
one succeeds. Each role must declare `tool_permissions`. A generated profile
denies model tool commands the credential, its per-run link and the lease
directory. A consumer argv guard refuses Codex command-line overrides, such as
`--sandbox`, that would silently replace that profile. After every run the
supervisor scans the run directory for credential links; a finding, or a scan
that cannot complete, quarantines the run (1.3.1). Entry points that do not route
through it are unmanaged and stay blocked for shared-login use. Examples are a
workstation launcher that exports a fixed `CODEX_HOME`, or the harbor-ablation
Rust harness run on its own; its README lists the known cases.
Its runbook covers the live sequence, failure recovery and the re-login procedure.

GPTgrep's native roles request `gpt-6-luna`, reasoning effort `max`, service tier
`fast`. The guarded protocol normalizes Fast to `priority`; that request does not
prove the served tier. Use the explicit `--codex-home`/`--ocr-codex-home` options
or corresponding control fields, and separate state/log/temp directories where
supported. Named permission profiles must be configured and actually qualified
for the selected runtime; do not infer filesystem isolation from a profile name.

## OpenRouter Decisions

Provide `OPENROUTER_API_KEY` only to the invoking GPTgrep process through your
secret manager. Do not place its value in a command example, JSON control or
checked-in environment file. The default model is
`openai/gpt-6-luna-decisions`; `typesafe/jev-1.13` is an explicit text-only legacy
choice. An OpenRouter alias such as `~typesafe/jev-latest` is also text-only. It is
admitted only when the served build has the same author and family, for example
`typesafe/jev-1.13-20260917`. Codex authentication and the Decisions credential
are independent.

```sh
"$GPTGREP_BIN" search 'How are snapshots retained?' ./evals/fixtures/corpus \
  --mode hybrid --json
"$GPTGREP_BIN" ask 'How are snapshots retained?' ./evals/fixtures/corpus \
  --codex-home "$GPTGREP_CODEX_HOME" \
  --model gpt-6-luna --reasoning-effort max --service-tier fast --json
```

These are live, model-assisted commands. Failures remain explicit, and unknown
usage/cost remains null. A failed `judge` returns code `decisions_judge_failed`
with a sanitized `judge` object. That object has the requested and served models,
whether a send started, usage/cost or a null reason, and latency. Without
`--decisions-budget`, the product uses a finite single-invocation context. A public
checkout does not inherit another operator's budget or existing campaign controls.

### Shared submission budget

`--decisions-budget FILE` only joins an existing matching ledger; it never creates
a campaign or renews consumed allowance. The run owner initializes a new ledger
once, before the run, in a private directory:

```sh
install -d -m 700 "$RUN_DIR"
"$GPTGREP_BIN" decisions-budget init --run-id "$RUN_ID" --plan-file "$PLAN" \
  --deadline-seconds 1800 --max-submissions 2 \
  --ledger "$RUN_DIR/ledger.json" --output "$RUN_DIR/budget.json"
"$GPTGREP_BIN" --decisions-budget "$RUN_DIR/budget.json" judge --input "$REQUEST" --json
"$GPTGREP_BIN" decisions-budget status --control "$RUN_DIR/budget.json" --json
```

`init` calls `ExecutionContext::new(run_id, sha256(plan file), now + N seconds,
max_submissions)` and attaches the ledger with `with_new_ledger`, the create-only
form of `with_ledger`. Both paths must be absolute. The ledger, its `.lock`
sidecar and the control file must not exist; init never joins, resets, renews or
replaces them. The lock records the immutable header and the original
same-boot clock anchor. The control file (`gptgrep.decisions-budget-contract.v1`)
is written completely with mode 0600 and published without replacement. A second
`init` for the same paths fails and leaves the ledger and control unchanged. Use a
new ledger for a new run.

`--deadline-seconds` accepts 1–86,400 (24 hours) and is never renewed.
`--max-submissions` accepts 1–200. The plan file must be a regular file of at most
1 MiB. The clock anchor is bound to the boot and clock namespace that created it.
After a reboot, or a container VM restart for containerized runs, every join fails
closed and `status` reports `clock_binding_unavailable`; create a new ledger for a
new run.

`status` reads the control, lock and journal without locking or writing. It
reports reservations by outcome (`reserved`, `in_flight`, `unknown`, `terminal`),
`remaining_submissions`, occupied slots, requested selectors and
`deadline_state` (`open`, `expired` or `clock_binding_unavailable`). It also
reports `admission_open`, and it stays readable after the deadline. Every
reservation is permanent, including failed and unknown outcomes. A joined process
therefore stops at `max_submissions` with `Decisions submission budget exhausted`
before any POST. The ledger bounds submissions, not dollars. Enforce a spend cap
outside the product: multiply the remaining submissions by a reviewed worst-case
price per call. Optionally also set a provider-side key limit.

## Native parsing, OCR and complete index publication

The default legacy `index` remains local. Explicit `--rich` / `--ocr off` publishes
the rich path only when all required content is available; required but disabled
OCR is a failure. A zero-model preparation is available as:

```sh
"$GPTGREP_BIN" index /absolute/owned/documents --rich --plan-only --json
```

The current native Document/OCR execution path uses caller-selected native tools,
Skills, permissions and job budgets. Set `DocumentAgentControl.native_execution`
and an OCR profile built with `OcrRuntimeProfile::with_native_agent`. Their optional
`native_runtime_package` selects an absolute package root and exact manifest
SHA-256. [Runtime staging](../runtime/codex/STAGING.md) describes the four Skills,
three roles, actual working-directory projection and explicit role bindings.
The [incur CLI](../packages/cli/README.md) exposes the same native operations;
[Rust API composition](rust-sdk.md) provides a direct API example.

For retained fixed-image execution, build the derivative at
[patches/codex-native-viewer](../patches/codex-native-viewer/README.md). Its upstream
pin is Codex `rust-v0.162.1`, commit
`092d3acd6bec3e3a14bdc7e7a2810ab628ab759d`; the derived version is
`0.162.1-gptgrep.1`. Keep the source patch, build identity and licenses together.
Page and Document image qualifications bind the actual binary and profile;
another build cannot reuse the owner's private acceptance by copying a digest.

The advanced workflow requires operator-created inputs:

| Input | Contract and purpose |
| --- | --- |
| Parse control | `gptgrep.parse-agent.control.v1`: exact source aliases/pins, staging, resource/toolbox bounds, and optional original OCR context; see the [CLI control types](../crates/gptgrep-cli/src/parse_agent.rs) |
| Document control | One source-bound `DocumentAgentControl`: selected standalone binary, account/state, permissions, runtime profile, native execution selection and limits; see [driver types](../crates/gptgrep-host/src/parse_agent_driver.rs) |
| Index registry | `gptgrep.parse-agent.index-controls.v1`: exact parse-control pin and one document-control pin per selected supported source |
| OCR runtime/phase | Native-agent runtime profile or accepted legacy Page profile, plus the original-clock OCR phase/ledger shared across the document invocation |

The source provides schemas, the `native_execution_profiles` Rust example for
deterministic native profile metadata, and legacy qualification frontdoors.
Callers provide their own pinned binary, permissions, account, source controls and
job configuration. The following commands show the actual interface after that work;
the variables refer to your own existing controls and their SHA-256 values.

```sh
"$GPTGREP_BIN" parse-agent serve \
  --config "$PARSE_CONTROL" --expected-control-sha256 "$PARSE_SHA"

"$GPTGREP_BIN" parse-agent run \
  --config "$PARSE_CONTROL" --expected-control-sha256 "$PARSE_SHA" \
  --source "$SOURCE_ALIAS" --mode off --output "$NEW_PRODUCT"

"$GPTGREP_BIN" parse-agent agent \
  --config "$PARSE_CONTROL" --expected-control-sha256 "$PARSE_SHA" \
  --document-agent-control "$DOCUMENT_CONTROL" \
  --expected-document-agent-control-sha256 "$DOCUMENT_SHA" \
  --source "$SOURCE_ALIAS" --output "$NEW_AGENT_PRODUCT"

"$GPTGREP_BIN" index "$DOCUMENT_ROOT" \
  --parse-agent-config "$PARSE_CONTROL" \
  --expected-parse-agent-config-sha256 "$PARSE_SHA" \
  --document-agent-controls "$INDEX_REGISTRY" \
  --expected-document-agent-controls-sha256 "$REGISTRY_SHA" --json
```

`serve` is foreground NDJSON for an external PTC caller; it waits for requests.
`run` executes deterministic stages and optionally selected OCR when `--mode auto`
is explicitly selected. `agent` runs one native Document turn over the toolbox.
Those three produce no generation themselves; the last command imports complete,
verified owned products and atomically publishes the generation. `parse-agent
qualify` records private image evidence, not an automatically accepted profile.

Processing windows bound intermediate work without a whole-document page-count
cap. Every source page, metadata inventory and required OCR result must be covered
before publication. Logical buffer/staging reservations are not process RSS or
physical disk measurements. Native execution uses the caller's tool, timeout,
output, OCR admission and staging budgets. Fixed Page/Document profiles retain
their original limits. The effective deadline is the smallest applicable
document, frontend/session and original phase deadline; source completeness is
checked independently of how much metadata the model inspected.

After a successful rich generation, `search --multimodal`, `ask --multimodal`
and `batch --rich` consume its source-bound text/images. Ordinary queries do not
start OCR. See [rich batch](rich-batch.md), [native metadata](native-metadata.md),
[rerank](rerank.md) and the separate [foreground OCR service](ocr-serve.md).

## Development checks

Use the selected external environment and retain failures as well as final passes:

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
uv venv --python 3.13 "$GPTGREP_DEV_ROOT/eval-venv"
uv pip sync --python "$GPTGREP_DEV_ROOT/eval-venv/bin/python" evals/requirements.lock
"$GPTGREP_DEV_ROOT/eval-venv/bin/python" -B -m unittest discover -s evals -p 'test_*.py'
"$GPTGREP_DEV_ROOT/eval-venv/bin/python" -B scripts/eval.py \
  --binary "$GPTGREP_BIN" --output "$GPTGREP_DEV_ROOT/results/offline-eval.json"
"$GPTGREP_DEV_ROOT/eval-venv/bin/python" -B scripts/check_runtime_staging.py \
  --script runtime/codex/stage.sh --package . \
  --output "$GPTGREP_DEV_ROOT/results/runtime-staging"
pnpm --dir packages/cli install --frozen-lockfile --ignore-scripts
pnpm --dir packages/cli test
```

When changing the harbor-ablation Rust harness (its own Cargo workspace, not
covered by `cargo test --workspace`), run its suite from
`workspace/harness-tooling/harbor-ablation` as its
[README](../workspace/harness-tooling/harbor-ablation/README.md) describes, with
`GPTGREP_MOCK_PYTHON`, `GPTGREP_JUDGE_TEST_PYTHON` and
`GPTGREP_JUDGE_TEST_PYTHON_SHA256` naming a qualified interpreter and its digest:
`cargo test --offline --locked --manifest-path harness/Cargo.toml -- --test-threads=1`.
When changing `workspace/harness-tooling/codex-auth`, run
`python3 -I -B tests/run_qualification.py --results-dir DIR` from that directory
and keep its summary.

Set pnpm's store to the selected external location using the local project/runtime
configuration before installation. The offline suite does not establish native
model behavior. A code change needs its relevant regression and the complete
release checks before release; a native/profile change also needs matching actual
runtime evidence. Do not count ignored or unrun tests as passes.

## Full62 paired SEARCH through Harbor

The current adapter is
[workspace/harness-tooling/harbor-ablation/full62](../workspace/harness-tooling/harbor-ablation/full62/README.md).
The parent Harbor directory also retains earlier adapter work; use `full62` for
the approved main comparison.

```sh
export GPTGREP_FULL62_SOURCE="$PWD/workspace/harness-tooling/harbor-ablation/full62"
export UV_PROJECT_ENVIRONMENT="$GPTGREP_DEV_ROOT/full62-venv"
uv sync --project "$GPTGREP_FULL62_SOURCE" --frozen
# Host test extra: the hashed Pillow 12.3.0 lock.
uv pip install --python "$UV_PROJECT_ENVIRONMENT/bin/python" --require-hashes \
  -r "$GPTGREP_FULL62_SOURCE/base-image/requirements.lock"
uv run --project "$GPTGREP_FULL62_SOURCE" --frozen gptgrep-harbor-full62 --help
uv run --project "$GPTGREP_FULL62_SOURCE" --frozen python -B -m unittest discover \
  -s "$GPTGREP_FULL62_SOURCE/tests" -v
```

`uv.lock` pins only the adapter's runtime dependencies (Harbor), while the
base-image probe test imports Pillow 12.3.0 on the host. Install it into the same
environment only from hashed requirements: the base image's `requirements.lock`
above, or a local extras file in the same `pillow==12.3.0 --hash=sha256:...`
format that lists the host wheel's hash. An exact `uv sync` removes packages
outside the lock, so reinstall after every sync.

An environment shared between checkouts has one editable install that points at
only one of them. When reusing such an interpreter for another checkout, run the
suite with `PYTHONPATH="$GPTGREP_FULL62_SOURCE/src"` (not with `python -I`, which
ignores it) and record the `gptgrep_harbor.__file__` it imported;
`tests/test_source_identity.py` fails if the suite imports any other source tree.

For a real run, follow the adapter's sequence: build and verify the common
Python/Pillow/Poppler/native-Codex foundation; add the complete Linux GPTgrep plus
PDFium bundle to the treatment derivative; build the treatment index inside that
Linux image at `/data/corpus` with its product binary, from the raw documents only
and before any question is exposed (materialization rejects an index whose pinned
manifest root is not `/data/corpus` or whose sources differ from the pinned
documents, such as a host-built index); qualify matching reader permissions and account mounts; pin
the owner-approved reader seccomp profile that both reader containers need for Codex's
bubblewrap sandbox (the adapter's `seccomp/` directory); supply
the frozen private cohort and Judge contract; select one treatment Decisions mode
(an existing shared ledger, per-trial preparation, or invocation-bounded contexts
with one create-new host claim per treatment trial, a pinned price and a cohort
cost cap that is checked at admission, not enforced at runtime); prepare all 124
tasks; build exact role images; run the generated Harbor job; aggregate all paired
outcomes. Its `prepare`,
`build-local-images` and `aggregate` commands require pinned manifests; `prepare`
prints its budget preflight before writing tasks. `harbor run -c GENERATED_JOB_JSON`
uses the same installed adapter environment and a fresh admitted output namespace,
with one attempt and no automatic retry.

Raw Codex receives only original documents and may extract/render during SEARCH.
GPTgrep receives the same bytes plus a frozen index built before question access.
Both readers use the same selected native binary and generic raw-document tools,
with identical configuration bytes that disable hosted web search, the host-side
connector, plugin and tool-suggestion features, and model-tool network access.
Both reader containers run under the same seccomp profile, which adds only the
namespace calls bubblewrap needs to Docker's default; the Judge keeps the default.
GPTgrep's reader contract (base instructions, citation and abstention rules and
answer rules) is part of the treatment; the raw-only reader receives the common
task prompt with Codex's default base instructions. In either reader mode the
primary raw-only prompt omits GPTgrep's answer rules and Skills. Freeze the actual
adapter input bytes; keep effective native base-instruction hashes unknown when
the runtime does not expose them.
The Judge runs in a separate environment with the complete answer and frozen
reference, not the reader's history. Task failures stay in the denominator;
unavailable judging stays unknown. INDEX and Judge costs are separate from
question-dependent SEARCH. See the [evaluation protocol](design/agentic-search-evaluation.md)
for the unchanged primary metric and missing-result rules.

The private 62-task cohort, accepted image/account controls, budget state and
prebuilt index are not distributed. Synthetic adapter checks can run from source;
they do not make a real paired run ready or prove an advantage. The full62 paired
comparison and full34 native PTC indexing remain open live gates; see the
[experimental status](experimental-status.md). The first paired run uses a frozen
zero-model text index. Build it inside the Linux treatment image at the
in-container corpus root, from raw documents only and before question exposure.
The product binds a generation to its canonical root, so an index built on the
host fails with `index identity mismatch; reindex at current root`.
