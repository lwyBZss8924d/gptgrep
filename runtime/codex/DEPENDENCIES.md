# Runtime dependencies

The Skills and role files contain product guidance. A runnable package also needs the actual executables and libraries selected by its image or job builder. Record resolved paths, versions, content hashes, and the checks actually performed in the private deployment inventory. Do not interpret a feature flag, filesystem path, or dependency declaration as successful execution.

| Component | Required when | Readiness evidence |
| --- | --- | --- |
| `gptgrep` | All CLI workflows | Selected binary hash; `--help`, `--llms`, `--schema`, `doctor --json` |
| `codex` and matching `codex-code-mode-host` sibling | Native agent workflows; code mode when selected | Matched build/package provenance, binary hashes, selected runtime capabilities, successful actual native invocation |
| LiteParse CLI and its Node/runtime dependencies | The selected parser requires them | The parser's actual configured executable and runtime paths; successful selected source extraction |
| LibreOffice | Office source conversion | Actual configured converter and successful bounded conversion |
| Python 3 with POSIX descriptor-relative filesystem support | Optional standalone `runtime/codex/stage.sh` helper | Actual interpreter and successful staging/refusal checks; the native Rust staging path does not require Python |
| Shell and requested language/image utilities | Native jobs that use them | Executable paths and successful task-level operations; include Python/Pillow only if selected and installed |
| GPTgrep Rust SDK source, Cargo/rustc, linker and required libraries | Direct public Rust API composition | Pinned source and lockfile, actual toolchain paths, selected-target build and execution; binary-only presence is insufficient |
| Optional `gptgrep-ai` incur wrapper and its declared JavaScript runtime | Wrapper CLI composition | Actual installed package/runtime and help/schema; do not infer Node or a Rust FFI from wrapper source |
| Decisions provider access | Default hybrid search, ask, summarize, and explicit paid operations | Selected endpoint/model, actual requests and receipts; credentials remain in the process environment |

The PageIndex, tgrep-core, and typed Decisions integrations are GPTgrep library components; do not add fictitious executable dependencies for them. A bare Codex comparison runtime should share its intended tools and dependencies while excluding GPTgrep-specific Skills, role instructions, and supervisor guidance.

Build dependencies into the selected isolated image or supply admitted external executables through the existing runtime configuration. Preserve OS networking policy and the caller's permission mode. An image's `--yolo` selection, where explicitly requested, belongs to its launch configuration and is not written into these portable assets.
