# Stage the GPTgrep guidance package

Ship the repository's `skills/` and `runtime/codex/` directories together with the executable distribution. They contain no user-home paths or account configuration. The standard native layout for an owned job is:

```text
TASK_CWD/
  AGENTS.md
  .agents/skills/gptgrep/SKILL.md
  .agents/skills/gptgrep-document/SKILL.md
  .agents/skills/gptgrep-ocr/SKILL.md
  .agents/skills/gptgrep-retrieval/SKILL.md
  .codex/agents/gptgrep-document.toml
  .codex/agents/gptgrep-ocr.toml
  .codex/agents/gptgrep-retrieval.toml
```

Use the actual native process working directory, including each fresh page OCR directory. With no selected project-root marker, native Skill discovery is limited to the current directory; staging only in an ancestor scratch directory is insufficient. Do not create a fake Git marker to change that boundary. The standalone staging command requires Python 3 on a POSIX system with descriptor-relative filesystem operations. It refuses existing instruction or configuration destinations, verifies and retains all eight source assets before writing, and creates destinations exclusively through held directory descriptors without following symlinks. The task directory must be owned by the caller and its path components must not be symlinks. A detected concurrent path replacement fails closed; files already created in the owned task directory may remain after failure:

```sh
sh "$package_root/runtime/codex/stage.sh" "$package_root" "$task_cwd"
```

For a container image, copy both source directories to a declared package root such as `/opt/gptgrep/share`, then invoke the same staging command for each fresh task workspace. Install and verify the selected executables through the image's existing build; the staging command performs no download, installation, provider call, or native launch. Record the manifest hash and staged file hashes with the job.

The top-level `AGENTS.md` is the customizable supervisor prompt. Each role TOML supplies its customizable `developer_instructions`; the four Skills hold progressively selected workflow guidance. Edit an owned package or job copy, regenerate its asset hashes, and record that effective version before starting the task. Keep durable `AGENTS.md` under native discovery. Put any separate per-job instruction overlay into the caller's existing native instruction channel and record its hash; do not duplicate the full `AGENTS.md` in both channels. These files intentionally omit model, reasoning effort, permission, and tool settings so the caller's runtime configuration remains authoritative.

## Native binding

Start the native runtime in the staged job directory using its existing account and isolated state arrangement. Make the `.codex` project layer available through the selected runtime's normal project configuration and trust contract; a copied role file alone does not prove the role was loaded. An existing `.codex` directory can supply a project layer without a `config.toml`, but an unknown or untrusted project layer may be disabled. Do not change account-home trust as a staging side effect. The launcher may explicitly select each role file through its normal job-scoped native agent configuration. Observe startup warnings and the actual native role/tool inventory.

For each explicitly selected Skill, verify the canonical regular `SKILL.md` and expected content hash. Ask the selected app-server for:

```json
{"method":"skills/list","params":{"cwds":["/absolute/job/cwd"],"forceReload":true}}
```

Check the returned entry for that cwd, the exact Skill name and path, `enabled: true`, and relevant load errors. The JSON is a protocol fragment; supply the normal request ID and transport envelope in the client. Native `turn/start` input includes both the task text and an explicit Skill input:

```json
[
  {"type":"text","text":"Use $gptgrep-document for the supplied document task."},
  {"type":"skill","name":"gptgrep-document","path":"/absolute/job/cwd/.agents/skills/gptgrep-document/SKILL.md"}
]
```

Use `gptgrep` for a supervisor task, `gptgrep-document` for a document task, `gptgrep-ocr` for a page OCR task, and `gptgrep-retrieval` for a retrieval reader. A Skill catalogue response proves discovery; explicit invocation and retained runtime evidence establish the attempted instruction load. Native execution and useful task completion remain separate checks. Record selected Skill hashes, discovery results, actual turn input, and any runtime warnings without copying credentials or unrelated private configuration.

Role files use standard standalone `.codex/agents/*.toml` metadata: `name`, `description`, and `developer_instructions`. Select the matching role through the runtime's native delegation interface. They are available choices, not a requirement to spawn a worker for every operation.

Keep GPTgrep assets out of a bare Codex comparison workspace. Use separate owned task directories for distinct agents, and retain the comparison's intended dependency parity.
