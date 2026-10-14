# Agentic SEARCH evaluation and convergence

This document defines the main GPTgrep evaluation protocol. The executable run
configuration, remaining implementation and actual comparative evidence are
separate deliverables; adopting this protocol does not establish their completion.
The retained PageIndex first-release gate remains separate.

The current executable adapter is [full62 Harbor](../../workspace/harness-tooling/harbor-ablation/full62/README.md).
Use [development setup](../development.md) for its environment and
[experimental status](../experimental-status.md) for observed progress. As of
2026-10-13, its complete paired run and Judge remain pending; source and synthetic
interface validation do not satisfy this protocol.

## Main question and two arms

Does the same Codex agent answer document questions better with GPTgrep than with
its ordinary tools and raw documents alone?

| Condition | Bare Codex | Codex with GPTgrep |
| --- | --- | --- |
| Initial material | Raw documents only; no prepared text, OCR, tree or index | The same raw documents and a frozen GPTgrep index built from raw documents alone |
| Reader | Matched Codex version, model, effort, service tier and sandbox resources; Codex's default base instructions plus the common task prompt | The same Codex runtime and resources, with GPTgrep's reader contract as base instructions (answer/citation/`insufficient_evidence` schema; citation, abstention and answer rules) plus the GPTgrep tools |
| Normal tools | Normal permitted file search, reading, parsing and rendering tools | The same normal tools plus GPTgrep |
| Task and scope | The same question and protocol-permitted document scope | Identical |
| Initial state | A fresh reader without prior answers, judge state or the other arm's trace | Identical isolation |
| Preparation cost | Excluded from this comparison | All build, parse, rendering, OCR, enrichment, tree and index preparation costs excluded |
| Query work | All work after task delivery, including any on-demand document processing | All question-dependent retrieval, routing, Decisions, evidence delivery and answering |

The default matched runtime is `gpt-6-luna / max / fast`; freeze the actual
selected reader and judge settings before running. Preserve requested and observed
model/tier identities. Match resource limits and concurrency and interleave the
two arms in randomized task pairs or time blocks.

Freeze an explicit **treatment feature profile**: the exact GPTgrep commands and
preparation mode; whether real routing and reranking are enabled, their provider
and model; and which tree/structure, text, OCR and visual evidence paths are
available. Report separately what the actual trace used, failed to use or could
not observe. The existing Harbor pilot with deterministic rich-batch tools and
no Luna Decisions establishes protocol/tool-use development evidence only. It
does not establish the full routing/reranking or System One helper capability.
A treatment-only run, even over all 62 tasks with live Decisions, is development
evidence. Without the matched raw-only arm it supports no `Q_gptgrep - Q_bare`
claim, and the tasks it exposes remain development material.
A feature present in the binary or index is not evidence that the run used it.
This is honest configuration labelling, not a requirement that every mechanism
be enabled or independently win; claims must stay within the frozen profile.

GPTgrep's reader contract is a declared part of the treatment (owner direction,
2026-10-13 UTC). A result therefore compares the GPTgrep harness, including that
contract, with bare Codex; it does not isolate retrieval. For the paired run, the
raw-only arm receives only the common task prompt and Codex's default base
instructions, without GPTgrep's answer rules or Skills in either reader mode.
Freeze actual adapter input fingerprints and distinguish them from expected
product contracts or effective native base instructions that are not observable.
An optional
bare-Codex-plus-answer-rules diagnostic arm can separate prompt-contract effects;
it is not part of the primary claim.

This estimates the difference between two real product configurations. The
prebuilt-index treatment and raw-only baseline do not identify the isolated
causal contribution of a search algorithm. Do not give the baseline prepared
GPTgrep material to make the inputs appear symmetric, and do not deduct the
baseline's on-demand parsing from its timed query work.

Start timing when the question becomes available to the system. A host-side,
question-dependent helper executed before the reader still belongs to the query
stage. Excluded INDEX preparation must not receive evaluation questions, gold
answers, evidence annotations or prior verdicts.

## Cohort and claim selection

The default compatibility cohort is the complete pinned PageIndex-OSS-Benchmark
set: **62 tasks across 34 documents**. It is historical development material,
not an unseen holdout. A new frozen two-arm execution on these tasks supports a
claim about this cohort; it does not erase earlier exposure. A small pilot cannot
replace the declared full cohort.

A larger or genuinely unexposed MMLongBench-Doc-V2 cohort is an optional future
extension, not a prerequisite for this comparison. Select such material by
predeclared metadata criteria and group development/holdout membership by
document, including duplicate or revised versions of the same source where
known. Preserve previous split and exposure records. Do not rename an old pilot
or previously inspected task as confirmatory holdout evidence.

Freeze one complete configuration before scoring: cohort identity, reader and
judge settings, tools and instructions, source and binary revisions, treatment
index generations, limits, arm order, retry policy, metric and stopping rule.
Do not choose the best answer per task across runs.

**The default preregistered primary criterion is higher task-level answer
equivalence:** `Q_gptgrep - Q_bare > 0`, on the complete frozen denominator.
Inspect paired disagreements to understand the net gain. Report its absolute
size even when it is only one additional correct task.

An efficiency claim requires a **separate configuration selected before results**:
observed answer quality must be at least the baseline, and exactly one resource
endpoint must have been chosen in advance:

- Lower query cost per correct answer: all selected query-attempt costs divided
  by confirmed correct answers, including allowed failures and retries. A zero
  correct-answer denominator or missing required cost observations makes this
  metric unavailable, not zero or a win.
- Lower failure-aware time to a correct answer, frozen in advance. Use the mean
  capped time: a confirmed correct answer completed before its declared deadline
  contributes its actual duration; an incorrect answer or operational failure
  contributes that deadline. Pairwise deadlines must match. Judge unknowns stay
  unresolved, not automatically capped failures. If the treatment has no confirmed
  correct answers, this efficiency endpoint is unavailable; a zero-correct
  baseline remains comparable at its declared deadlines. Report actual wall
  time separately: the capped endpoint is not a claim that a fast failure
  physically ran until the deadline.

Do not switch to whichever endpoint wins after seeing the results. The default
quality route does not silently fall back to efficiency when quality ties.
Quality gains with higher cost or latency remain quality gains, not universal
dominance. No arbitrary weighted score, fewer tool calls, mandatory significance
threshold, power target, per-slice win or PageIndex third arm defines success.

## Scoring and missing outcomes

Use the existing benchmark answer-equivalence rubric and answer-format rules.
For a frozen denominator `N`, completed and judged task outcomes are `y_i` in
`{0,1}`, with `Q = sum(y_i) / N`. Preserve the four paired categories: both
correct, GPTgrep-only correct, baseline-only correct and both incorrect.

Startup failure, timeout, missing answer or unrecovered reader failure contributes
zero on that task. A failed run's empty response must not be scored as a correct
abstention. Only a completed intentional abstention follows the benchmark's
answerability rubric; report answerable and unanswerable slices separately.

Judge failure is **unknown**, not an incorrect model answer and not an omitted
task. Keep it in `N`, report the count, and show accuracy bounds and paired
sensitivity to unresolved labels. Do not publish a point estimate as complete
accuracy when judgments are missing. Resolve the judgments on retained answers,
or state whether the selected conclusion is robust to every permissible unknown
outcome. If unresolved labels can reverse the conclusion, the benefit remains
undetermined. This does not require replaying the reader.

Join both arms to a frozen unique task manifest rather than scoring only output
rows. Reject duplicate and unmatched predictions. The upstream MMLongBench scorer
joins by document and normalized question, omits missing predictions, and computes
metrics on judged, non-defective rows. Its resume and judge-failure semantics
also differ from a complete paired experiment. Preserve upstream scores as
labelled compatibility results alongside fixed-denominator outcomes; do not
silently inherit those omissions.

## Grounding and common evidence

Answer equivalence is the primary metric. It is not proof that the answer is
supported by the supplied document. Conversely, valid citation coordinates or a
high evidence-page recall do not establish answer correctness or entailment.

Use grounding as an auxiliary measurement only where both arms can be assessed
under a common source contract. A common citation is a document/page/quote or
equivalent publicly defined location. The baseline need not produce GPTgrep
issued IDs. GPTgrep generation, issued-reference, freshness and image-delivery
checks remain product-specific diagnostics, not asymmetric baseline penalties.

If both arms are asked to provide common evidence before the run, separately
report citation resolvability, support for material claims, unsupported claims
and answer completeness. Where annotations exist, report evidence-page recall
and ranking metrics appropriate to those labels. Do not invent graded relevance
for NDCG, assume annotations are exhaustive, or penalize a valid alternative
source as unsupported merely because it is absent from an evidence-page list.
Missing common evidence support is unknown unless the protocol explicitly made
it a required output of both arms.

The existing reader answer schema and original judge verdict contain an answer
and `equivalent`, `abstained`, `reason`; they do not themselves supply a semantic
grounding verdict. A joint `equivalent AND supported` diagnostic requires a
separately declared common support rubric. Do not add it after observing outcomes
or silently substitute it for the primary metric.

The original FACTS Grounding method supplies long-form document text, checks
whether the response fulfils the request, then checks response-level source
support; ineligible responses do not disappear from its score. Its preparation
excluded PDFs with unreadable OCR. It therefore does not establish retrieval,
INDEX or OCR quality. [Original methods, sections 2–3](https://arxiv.org/html/2501.03200v1)

The later FACTS paper is a four-component suite: Multimodal, Parametric, Search
and Grounding. Its revised Grounding v2 uses different prompts and Gemini 2.5
Flash/GPT-5 judges. Do not import its suite score or label a local Codex diagnostic
an official FACTS result. [Later methods, sections 2–6](https://arxiv.org/html/2512.10791v1)

A minimal local adaptation can reuse frozen answers in the existing separate
judge path and record `eligibility`, `grounding`, `source_coverage`,
`evidence_integrity` and `judge_status`. These are proposed local fields, not the
official FACTS schema. Give the blinded judge equal access to the authoritative
source scope for both arms; judge-only normalization must not reach the reader.
Missing source coverage or failed judging yields unknown, not unsupported or
supported. Keep eligibility, semantic support and evidence integrity separate:
a true supported statement can miss the question, and a valid reference can
still support a false claim. Report eligible/supported/unresolved counts and
their denominators without changing primary equivalence. This auxiliary
diagnostic requires neither a new dataset nor the historical judge ensemble and
adds no must-win gate.

## Judge validation and blinding

Freeze judge model, effort, tier, prompt, schema, response truncation and recovery
rules. Bind each verdict to the exact answer, rubric and judge configuration;
task identity alone is insufficient for reuse. The reader cannot access gold,
judge prompts, prior verdicts or answers from another task.

Validate the scorer on independent public or synthetic controls: equivalent
paraphrases, wrong values or units, missing answers, intentional abstentions and
unsupported claims. Keep answer correctness and evidence support separate.
Blind the main judge to arm names, tools and machine paths while preserving the
actual answer. A common grounding judge needs access to the same underlying
source scope for both arms, not privileged GPTgrep-only evidence.

Review paired disagreements and a preselected random sample independently and
blindly. Report judge disagreements and whether adjudication changes the result.
Do not repeatedly try judges and keep the one favoring a preferred arm. Preserve
unavailable judgments and all scoring attempts.

## Query metrics and uncertainty

Report quality, latency, tokens and cost separately and display their Pareto
tradeoffs. Query wall time includes startup, local tools, index verification,
serialization, routing, reranking, model reading and final-answer production
after task delivery. Use observed spans for stage decomposition; overlapping
durations are not additive wall time. Preserve timeouts and failure stages.

Record Codex usage and requested/observed profiles, Decisions logical
submissions, provider responses, available cost, and missing observations.
Physical provider requests that are not observable remain unknown. Any dollar
comparison uses a frozen common price basis and is labelled an API-price
equivalent, not an observed ChatGPT subscription bill. Judge cost is separate.
All INDEX costs remain outside the comparison and cannot veto a SEARCH gain.

For paired uncertainty, retain every question pair within its document group.
Use document-grouped resampling when enough independent groups exist, reporting
the task-weighted effect and the number of groups. Document-macro averages are
separate diagnostics. With one or very few groups, intervals can be degenerate
or not estimable; report that limitation and the paired outcomes instead of
manufacturing precision. Repeats of one task are not new independent tasks.

Uncertainty qualifies generalization; it is not an extra hard Goal gate. A
62-task result changes in increments of about 1.61 percentage points. One net
additional correct task is a measured cohort difference, not evidence of a
universal advantage. For broader future claims, use developmental discordance
and document clustering to plan sample size and interval precision. Do not
require every slice to be significant.

## INDEX and mechanism diagnostics

The required comparison has only two arms. Run optional diagnostics when a real
failure or gain needs explanation; no component must win independently.

| Question | Minimal useful diagnostic | Qualification |
| --- | --- | --- |
| Does lexical retrieval miss useful candidates? | Fixed index, explicit lexical candidates versus an independent structure/tree recall path with the later reader and budget held fixed | A changed route and candidate pool is a strategy comparison, not pure reranking |
| Does Decisions improve ordering? | Identical source-bound candidate pool and content; deterministic order versus Decisions order | Scores and probabilities are not calibrated task correctness |
| Does visual evidence help? | Where supported, hold the generation and search settings fixed and vary actual image delivery on development tasks | Metadata or encoded image bytes alone do not establish image consumption |
| Does OCR make documents usable? | Same raw inventory, OCR off versus automatic routing, recording publication and failed-page outcomes | Current rich ingestion refuses publication when required OCR is disabled; this is availability diagnosis, not two successful index-quality arms |
| Does enrichment help? | Use a consumer that actually reads the chosen overlay, with the planner and reader otherwise fixed | Current rich multimodal query paths do not establish consumption of ordinary navigation overlays; do not claim that a full rich-tree overlay arm already exists |

Keep index diagnostics outside the success criterion: ingestion coverage;
required, executed, reused and failed OCR tasks; artifact freshness; publication
atomicity; structure/text/rendition coverage; storage; and preparation resources.
Text density and metadata presence are proxies, not proof that every fact was
extracted. OCR accuracy requires separate known references; do not invent it
from a successful model turn. A no-op auto-OCR run is a negative control, not
an OCR improvement. A stored but unused overlay cannot cause a retrieval gain.

The reference `jev-doc-search` pipeline suggests a useful failure decomposition:
section beam, page-window choice, then verification. Inspect candidate coverage
before blaming the final reader. Its pruning, truncation and fallback rules
limit what can be inferred; do not transfer its model, retry policy or missing
usage assumptions into GPTgrep.

Keep existing negative results visible. A small public synthetic text experiment
measured a twelve-operation batch faster than twelve serial invocations, while
`rg` remained faster than both. It is evidence about local invocation overhead,
not a document-QA or universal search win. Engine timing compares identical
normalized bytes and output semantics; that auxiliary condition must not replace
the raw-only baseline of the main comparison.

## Execution sequence and stopping

1. Use a small public/synthetic paired pilot to validate collection, failure
   accounting, source access and judge bindings. It is engineering evidence.
2. Investigate live development failures with one falsifiable change at a time.
   Optional mechanism diagnostics serve this step, not a compulsory matrix.
3. Freeze one candidate and its default quality claim, or a separately
   preregistered efficiency claim. Run the complete declared paired cohort.
4. Score retained outcomes, check disagreements, report unknowns and uncertainty,
   then state whether the selected cohort criterion passed. Stop at the frozen
   cohort or budget, not at the first favorable partial result.

If the criterion does not pass, retain that outcome and return to development.
Any subsequent run has a new frozen configuration; its history does not become
unseen. Broader fresh-cohort evidence can follow when needed and feasible.
Passing source checks or a single live example is not a completed comparison.

## Separate first-release gate and provenance

The same-Codex criterion replaces the PageIndex-superiority dependency of the
main Goal's comparative part. Existing D1–D7 implementation, verification and
source-delivery work still has to be completed: shared G0 source/license, G1
correctness/freshness/independent review, G2 actual native/provider outcomes and
G6 source/PoUW/backup obligations are not made release-only. A comparison win
does not declare unfinished code or delivery complete.

Main Goal convergence does not grant first-tag permission. The retained G3–G5
PageIndex comparison requirements still apply to the first release, including
the existing G5 complete-cohort quality, cost and independent-evidence conditions.
Historical cross-model and cross-judge qualifications remain intact. Neither a
PageIndex third arm nor that tag gate is required to decide the new same-Codex
SEARCH comparison.

Primary implementation references are [`SPEC.md`](../../SPEC.md),
[`goal-contract.json`](../../workspace/harness-config/goal-contract.json),
[`gptgrep_system_eval.py`](../../scripts/gptgrep_system_eval.py),
[`pageindex_eval.py`](../../scripts/pageindex_eval.py), and the existing
Harbor reader/judge source under `workspace/harness-tooling/harbor-ablation`.
Upstream methodology references are the
[PageIndex benchmark](https://github.com/VectifyAI/PageIndex-OSS-Benchmark),
[MMLongBench-Doc-V2 scorer](https://github.com/VectifyAI/MMLongBench-Doc-V2/tree/main/eval),
[Jev document search](https://github.com/VectifyAI/jev-doc-search), and
[tgrep benchmark methodology](https://github.com/microsoft/tgrep/blob/main/BENCHMARKS.md).
These are source-based design inputs, not newly measured GPTgrep outcomes.
