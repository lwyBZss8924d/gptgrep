---
name: gptgrep-document
description: Parse and inspect documents with GPTgrep, use native metadata and page images, request required OCR, finalize complete source-bound products, and publish complete indexes. Use for a prepared document-agent job or an explicit parsing or indexing request.
---

# GPTgrep document preparation

Determine whether the caller supplied an active prepared-document toolbox or assigned a top-level CLI task. For an active document job, use the supplied `gptgrep_parse.toolbox` and original control context. Keep the registered source alias, hashes, artifact identities, and deadline. Native shell, code execution, and image viewing remain available according to the selected runtime; their outputs do not substitute for a verified parser product.

For programmable work, use public Rust APIs from the installed `gptgrep-parse`, `gptgrep-pageindex`, and `gptgrep-core` source or SDK when its toolchain and dependencies are available. Read current public signatures before coding. Use those APIs for the required source processing or inspection while preserving the original prepared context and finalization checks. The CLI examples below are available interfaces, not a limit on native code composition.

## Active document job

1. Use `prepare` and `extract_native` for the assigned source. Native extraction must traverse every original page through the parser's processing windows.
2. Use `inspect_pages`, `inspect_artifacts`, and `read_metadata_window` to understand page coverage and structure. Follow source-bound continuations when the inspected inventory is incomplete. Distinguish complete parser coverage from the model's selected inspection windows.
3. Read useful text or images with `read_artifact`, `inspect_images`, or `crop`. Preserve original page identity and crop transforms. The selected native image tool may help inspect these images; an image path or metadata inventory does not prove visual inspection.
4. Inspect `inspect_ocr_plan`. Usable native text and decorative vectors need no automatic OCR. Missing text, uncovered filled-vector content, and independent annotation requirements may still require it. Request required work with `execute_ocr` in the original task context and inspect the typed result. Do not fabricate OCR completion or restart the job's ledger, quota, or clock.
5. Use `finalize` only after complete extraction, artifact accounting, and required OCR are satisfied. Preserve any explicit incomplete result. Use `close` to release the prepared source when the protocol calls for it.

Do not launch another `parse-agent agent`, `index`, or OCR listener from an already active document turn to recreate its job. The caller owns admission, cross-source coordination, and publication. A complete product is an input to the generation writer; it is not an indexed generation.

## Top-level parsing and indexing

Discover exact arguments with `gptgrep parse --help`, `gptgrep parse-agent --help`, `gptgrep index --help`, and `gptgrep --schema`. For ordinary local parsing:

```sh
gptgrep parse "$source_file" --json
gptgrep index "$source_root" --json
gptgrep status "$source_root" --json
```

For an operator-prepared native document job, use the selected pinned controls:

```sh
gptgrep parse-agent agent \
  --config "$parse_control" --expected-control-sha256 "$parse_sha256" \
  --document-agent-control "$document_control" \
  --expected-document-agent-control-sha256 "$document_sha256" \
  --source "$source_alias" --output "$product_file"
```

For complete typed indexing, the caller supplies a control registry covering the complete supported corpus:

```sh
gptgrep index "$source_root" \
  --parse-agent-config "$parse_control" \
  --expected-parse-agent-config-sha256 "$parse_sha256" \
  --document-agent-controls "$document_registry" \
  --expected-document-agent-controls-sha256 "$registry_sha256"
```

These variables are caller-selected paths, aliases, and actual file digests. Do not infer or manufacture them. Keep native typed-index flags separate from legacy rich/OCR and merge options, as required by the current CLI. Use one original invocation across sources and publish only after final source inventory and hash checks. A resource limit must refuse incomplete publication rather than silently shorten the corpus.

Return the source and product or generation identity, complete page/artifact accounting, required OCR status, actual model inspection coverage, unsupported source counts, and failures. Keep index-building inputs limited to raw documents; questions, answers, and judging annotations are not ingestion inputs.
