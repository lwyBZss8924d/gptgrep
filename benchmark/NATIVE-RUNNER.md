# Native reader launch contract

`src/native.ts` drives the official Codex app-server over stdio. Both reader arms
use its normal native tool surface. It does not replace base instructions, add
dynamic tools, disable hooks, copy credentials or select an alternate model.
`model/list` must contain the exact requested supervisor and configured worker
models. The thread's model and permission-profile readback must match before a
turn starts. A backend reroute or observed worker model mismatch invalidates the
frozen model comparison.

Before admitting a new experiment, the controller must verify the
[latest official Codex OSS release](https://github.com/openai/codex/releases/latest).
Its private launch manifest records the observation time, release tag, asset
hashes and observed binary versions. Stage the CLI and all required companions,
including `codex-code-mode-host`, from that same release. An older qualified
runtime is historical evidence, not an eligible current base agent. Qualify
actual model-mediated file tools, PDF extraction/rendering and the declared
filesystem boundaries before launching benchmark readers or judges; CLI help,
successful model text and direct command execution alone do not establish this.
Keep the resulting runtime fixed across every arm and judge in the declared
experiment, then resolve latest again before the next experiment. Do not change
the requested model, reasoning effort or service tier as part of a runtime update.

`runReaderArm` is the child entrypoint. Invoke it once per arm inside a separately
admitted owner storage run and auth-supervisor role. The controller must create
one isolated task workspace containing only `context.json` and its full raw PDF
context. The context file has this portable shape:

```json
{"documents":[{"document_id":"source-id","path":"documents/source.pdf","sha256":"64-lowercase-hex-characters"}]}
```

Both arms use byte-identical mappings and PDFs, including context documents that
are not evidence for the answer. A treatment workspace may additionally contain
pinned, context-scoped products under `prepared/`; declare every relative path,
digest and the complete context document IDs. The treatment launch stages only
the eight pinned native runtime assets and `bin/gptgrep` from the pinned product
package. The bare workspace must contain no GPTgrep instructions, roles, skills,
binary or prepared products. Do not place gold, judge output, other tasks,
private controller files or the source checkout under either reader workspace.

To make a prepared generation discoverable, the treatment's `ReaderWorkspace`
may declare `preparedIndex` with `relativeRoot`, `generation` and
`manifestSha256`. The root must be `prepared` or a directory beneath it. Both
its `CURRENT.json` and selected generation manifest must be pinned prepared
artifacts, agree with that capability, and bind the full raw document context.
The runner checks those identities and all prepared artifact bytes before
exclusively staging a read-only `gptgrep-workspace.json` at the task root:

```json
{"prepared_index":{"root":"prepared","snapshot":{"generation":"g1-1","manifest_sha256":"64-lowercase-hex-characters"}},"schema":"gptgrep.workspace.v1"}
```

The descriptor contains no question, answer, document selection or judge state.
It stays outside the indexed root and leaves the common prompt and
`context.json` unchanged. Packaged supervisor instructions describe how to
discover it and preserve the snapshot. Without the capability the runner
stages no descriptor; final verification requires the exact descriptor bytes
when the capability is present and rejects the file otherwise. The bare arm
rejects both the capability and the descriptor.

The auth owner must supply a named permission profile, including its credential
denies. The runner requires `/` denied, the exact task/runtime read grants,
optional writes confined to the task workspace, optional `:minimal` system read
access, and disabled tool network by default. A declared Decisions provider can
use the exact `openrouter.ai` managed-proxy allowlist with all wider network
channels disabled; both arms must use the same declared network policy. The
provider key is a separate treatment-only capability. It verifies effective config and profile
readback; a legacy `sandbox` override is never sent. The current auth owner must
support the selected writable-workspace profile itself. A shared profile that
opens both arms, all tasks or evaluator files is refused. System executable and
library roots may be declared as common runtime reads when necessary; a home,
corpus, owner run root or controller output root is not a runtime read root.

The inherited auth supervisor must provide its run/source identities, isolated
Codex state, loopback refresh/revoke blockers, remote-control exclusion and
loopback proxy exclusion. The runner never opens an auth file. An explicit auth
failure writes the supervisor's declared failure record. The calling CLI must
exit **77** when the result has `authFailure`, so the supervisor can block further
consumption until its owner resolves authentication. Transport, capacity and
ordinary model errors must use a different failure exit. No next arm may start
after an auth failure or unconfirmed process cleanup.

`runReaderPair` is a serial controller with a required `launchArm` callback.
That callback launches a child through the storage owner and auth supervisor,
calls `runReaderArm`, waits for both owners' terminal receipts and returns the
child's result. It allocates that child's output directory within the child's
own admitted storage binding; the supplied controller-relative output location
is only a suggested organization and may be replaced at the launch boundary.
The two results must carry different auth run IDs and storage-binding digests.
Order is deterministically alternated from the frozen seed and pair index. Do
not implement the callback as two direct calls under the same environment.

`runNative` can also serve an independent judge, with its own exact workspace,
profile, prompt, model and output schema. Reader and judge state must never be
shared. Native traces contain bounded protocol metadata, input hashes, identity
readback and observed usage; raw config and stderr are not saved. Exact native
thread, session and turn IDs stay in private run evidence.

Usage is currently the root thread's observed cumulative usage only. Backend
request count, monetary cost and served model remain `null` unless the protocol
provides evidence; a thread/start model readback is not served-model evidence.
Worker model observations and backend reroutes are recorded separately. Private
activity records retain bounded, credential-redacted commands, exit status and
timing, without tool output. A command record is execution evidence, not proof
that its returned evidence was understood or used correctly. Include
failed and timed-out attempts and their observed usage in accounting. Do not
present root-only usage as complete multi-agent cost.

`runNativeForTest` accepts an in-memory transport and cannot launch a process.
It always marks results `executionMode: "test"`; those results are rejected by
the live pair controller. Fake protocol tests establish client behavior only.
They do not establish provider availability, sandbox enforcement, storage
adoption, auth correctness or live benchmark acceptance.
