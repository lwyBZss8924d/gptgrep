"""Runtime-only conversion of a pinned private cohort to 124 Harbor tasks."""
from __future__ import annotations

from pathlib import Path, PurePosixPath
import re
import string

from .common import (ARMS, MAX_READER_QUESTION_BYTES, MAX_TOOL_CALLS, PROFILE, canonical, copy_asset, digest, integer,
                     parse, pinned, put, relative, require, sha, tool_cap_parity)
from .native import JUDGE_OUTPUT_SCHEMA, output_schema_bytes
from .reader import task_prompt
from .runtime import (API_ENV, BASE_CAPABILITIES, BASE_TOOLS_PATH, CODEX_HOME, DECISIONS_BUDGET, DECISIONS_LEDGER_DIR, PASS_ENV, WORK_DIR, auth_compose, auth_paths,
                      base_tools_identity, model_config, native_identity, reader_seccomp_profile, reader_security_opt,
                      validate_asset_path)
from .product_bundle import PRODUCT_ENTRYPOINT, PRODUCT_PROVENANCE, bundle_identity
from .decisions import (CLAIM_ROOT, CONTROL_ROOT, FAMILY_ROOT, TRIAL_ROOT, WRITER_LOCK,
                        control as decisions_control, decisions_model, directory as decisions_directory,
                        invocation_cost_bound, selected_mode, shared_allowance)
from .trial_order import paired_order


# The bare arm has no tool-call cap; the stock CLI only observes native calls.
# In the treatment, native shell, image, MCP and sub-agent items count toward
# max_tool_calls together with GPTgrep calls, and exceeding it fails the Ask.
# Only the adapter maximum (MAX_TOOL_CALLS) is the parity choice; a lower cap is
# a declared, admission-bound deviation, and a cap below the floor is likely to bind.
TOOL_CALL_WARNING_FLOOR = 64
TOOL_CAP_DEVIATION = "treatment_tool_cap_below_adapter_maximum"


def tool_cap_deviation(value):
    """The exact qualification_mismatches entry that declares a treatment cap below the maximum."""
    return {"code": TOOL_CAP_DEVIATION, "treatment_max_tool_calls": value, "raw_tool_call_cap": "none"}


def tool_budget(value, qualification_mismatches=()):
    """Validate the treatment tool cap against the uncapped bare arm.

    Returns its budget_scope record and advisory warnings. A cap below the
    adapter maximum must be declared in qualification_mismatches, which the
    admission binds through the runtime subject; a stale declaration is
    rejected too.
    """
    integer(value, 1, MAX_TOOL_CALLS)
    declared = [entry for entry in qualification_mismatches
                if isinstance(entry, dict) and entry.get("code") == TOOL_CAP_DEVIATION]
    parity = value == MAX_TOOL_CALLS
    require(parity or declared == [tool_cap_deviation(value)], "treatment_tool_cap_deviation_undeclared")
    require(not parity or not declared, "treatment_tool_cap_deviation_stale")
    warnings = []
    if not parity:
        warnings.append({"code": ("treatment_tool_cap_may_bind_before_deadline" if value < TOOL_CALL_WARNING_FLOOR
                                  else TOOL_CAP_DEVIATION),
                         "max_tool_calls": value, "adapter_maximum": MAX_TOOL_CALLS,
                         "warning_floor": TOOL_CALL_WARNING_FLOOR})
    return {"treatment_max_tool_calls": value, "raw_tool_call_cap": "none",
            "adapter_maximum": MAX_TOOL_CALLS, "warning_floor": TOOL_CALL_WARNING_FLOOR,
            "parity": tool_cap_parity(value)}, warnings


def validate_cohort(cohort):
    require(set(cohort) == {"schema", "source_cohort_sha256", "tasks"} and
            cohort["schema"] == "gptgrep.harbor.cohort.v1", "cohort_schema")
    digest(cohort["source_cohort_sha256"])
    rows = cohort["tasks"]
    require(isinstance(rows, list) and len(rows) == 62, "full62_required")
    seen, documents = set(), set()
    for row in rows:
        require(set(row) == {"pair_id", "document_id", "question", "reference", "answer_format"}, "row_fields")
        for field in row:
            require(isinstance(row[field], str) and row[field] and len(row[field].encode()) <= 1048576,
                    "bounded_row_string")
        require(row["pair_id"] not in seen, "duplicate_pair")
        seen.add(row["pair_id"])
        documents.add(row["document_id"])
    require(len(documents) == 34, "document34_required")
    return rows


def image(value):
    # Docker config IDs bind local images without requiring registry publication.
    require(isinstance(value, str) and re.fullmatch(r"(?:[a-zA-Z0-9./_:-]+@)?sha256:[0-9a-f]{64}", value),
            "immutable_image")
    return value


def env_templates(values):
    require(type(values) is dict and len(values) <= 24, "environment_map")
    for key, value in values.items():
        require(re.fullmatch(r"[A-Z][A-Z0-9_]{0,79}", key) and
                isinstance(value, str) and re.fullmatch(r"\$\{[A-Z][A-Z0-9_]{0,79}\}", value),
                "environment_reference_only")
        require(key not in API_ENV, "api_auth_environment_forbidden")
        require(key in PASS_ENV or key == "OPENROUTER_API_KEY", "environment_key_not_allowed")
    return values


def toml_string(value):
    # JSON strings are valid TOML basic strings for the admitted ASCII metadata.
    import json
    return json.dumps(value, ensure_ascii=True)


# The in-container corpus root and the product's index identity (gptgrep-core):
# .gptgrep/CURRENT.json points to one generation whose manifest records its
# canonical root and every source digest. Legacy text and rich multimodal
# readers open different manifest schemas.
CORPUS_ROOT = "/data/corpus"
INDEX_SCHEMAS = {"legacy_text": "gptgrep.v1", "rich_multimodal": "gptgrep.manifest.v2"}
MAX_INDEX_POINTER_BYTES = 4096
MAX_INDEX_MANIFEST_BYTES = 128 * 1024 * 1024


def index_identity(runtime):
    """Check that the pinned index was built at the container corpus root from the pinned documents.

    The product binds a generation to its canonical root, so an index built on
    the host or at another root would fail every treatment Ask at query time
    ("index identity mismatch"). Only build state under .gptgrep/ is an index
    file; query-time host-attempts ledgers never are. Reads only index files.
    """
    rows = {row["relative_path"]: row["pin"] for row in runtime["index_files"]}
    require(all(path.startswith(".gptgrep/") and "host-attempts" not in PurePosixPath(path).parts for path in rows),
            "index_state_files_only")
    schema = INDEX_SCHEMAS[runtime["reader_mode"]]
    require(".gptgrep/CURRENT.json" in rows, "index_current_pointer")
    pointer = parse(pinned(rows[".gptgrep/CURRENT.json"], MAX_INDEX_POINTER_BYTES))
    require(type(pointer) is dict and pointer.get("schema_version") == schema and
            isinstance(pointer.get("generation"), str) and
            re.fullmatch(r"[A-Za-z0-9-]{1,128}", pointer["generation"]), "index_current_pointer")
    digest(pointer.get("manifest_sha256"))
    manifest_path = ".gptgrep/generations/" + pointer["generation"] + "/manifest.json"
    require(manifest_path in rows and rows[manifest_path]["sha256"] == pointer["manifest_sha256"], "index_manifest_pin")
    manifest = parse(pinned(rows[manifest_path], MAX_INDEX_MANIFEST_BYTES), MAX_INDEX_MANIFEST_BYTES)
    require(type(manifest) is dict and manifest.get("schema_version") == schema and
            manifest.get("generation") == pointer["generation"] and manifest.get("root") == CORPUS_ROOT,
            "index_root_identity")
    require(isinstance(manifest.get("documents"), list), "index_sources")
    sources = {}
    for entry in manifest["documents"]:
        source = entry.get("document") if schema == INDEX_SCHEMAS["rich_multimodal"] and type(entry) is dict else entry
        require(type(source) is dict and isinstance(source.get("path"), str) and source["path"] not in sources,
                "index_sources")
        sources[source["path"]] = source.get("source_sha256")
    # Exactly the pinned raw documents, byte for byte: nothing more, nothing stale.
    require(sources == {row["relative_path"]: row["pin"]["sha256"] for row in runtime["documents"]}, "index_sources")
    return {"schema_version": schema, "generation": pointer["generation"], "root": CORPUS_ROOT,
            "documents": len(sources), "manifest_sha256": pointer["manifest_sha256"]}


def judge_contract(judge):
    """Validate the frozen primary judge contract; return the Judge's exact schema-file bytes.

    Materialization calls this before creating any task; pre-admission
    validators can call it directly on the private contract.
    """
    require(type(judge) is dict and set(judge) == {"schema", "prompt", "response_schema"} and
            judge["schema"] == "gptgrep.harbor.primary-judge.v1" and isinstance(judge["prompt"], str),
            "frozen_judge_contract")
    fields = [field for _, field, _, _ in string.Formatter().parse(judge["prompt"]) if field]
    require(set(fields) == {"question", "answer", "answer_format", "response"}, "primary_only_fields")
    schema = judge["response_schema"]
    require(type(schema) is dict and schema.get("type") == "object" and
            type(schema.get("required")) is list and "equivalent" in schema["required"] and
            type(schema.get("properties")) is dict and
            schema["properties"].get("equivalent", {}).get("type") == "boolean" and
            all(type(v) is dict and v.get("type") in ("boolean", "string") for v in schema["properties"].values()),
            "supported_frozen_schema")
    # The Judge passes this schema as native strict structured output: every
    # property is required exactly once and no other key is allowed.
    required = schema["required"]
    require(schema.get("additionalProperties") is False and all(isinstance(name, str) for name in required) and
            len(set(required)) == len(required) and set(required) == set(schema["properties"]),
            "strict_output_schema")
    return output_schema_bytes(schema)


class Full62Adapter:
    def __init__(self, manifest, manifest_sha256):
        self.manifest_path = Path(manifest).absolute()
        from .common import read
        body = read(self.manifest_path)
        require(sha(body) == digest(manifest_sha256), "manifest_pin")
        self.manifest = parse(body)
        require(self.manifest["schema"] == "gptgrep.harbor.materialization.v1", "materialization_schema")
        self.cohort = parse(pinned(self.manifest["cohort"]))
        self.runtime = parse(pinned(self.manifest["runtime"]))
        self.judge = parse(pinned(self.manifest["judge_contract"]))
        self.rows = validate_cohort(self.cohort)
        # Validation output: bounded budget checks and advisory warnings. The
        # prepare command prints it before creating any task; the export keeps it.
        self.preflight = {"schema": "gptgrep.harbor.preflight.v1", "warnings": []}
        self._validate_runtime()

    def _validate_runtime(self):
        runtime = self.runtime
        require(runtime["schema"] == "gptgrep.harbor.runtime.v1" and runtime["profile"] == PROFILE,
                "runtime_profile")
        native_identity(runtime["native_cli"])
        auth_paths(runtime["auth"])
        require(runtime["codex_home"] == CODEX_HOME, "isolated_codex_home")
        require(runtime.get("native_ptc") is True and runtime.get("codex_permissions") == "full62_reader",
                "shared_native_reader_required")
        # Both reader roles run Codex's bubblewrap under the one owner-approved
        # seccomp profile. A missing, malformed, other or changed profile stops
        # here, before the preflight and before any task exists. It is a small
        # control pin, read like the admission, not a streamed task asset.
        pinned(reader_seccomp_profile(runtime.get("reader_seccomp_profile")))
        require(runtime["reader_mode"] in ("rich_multimodal", "legacy_text"), "explicit_reader_mode")
        if runtime["reader_mode"] == "rich_multimodal":
            require(runtime["jev_model"] in ("openai/gpt-6-luna-decisions", "openai/gpt-6-luna-decisions-20261006"),
                    "rich_decisions_model")
        else:
            # Every treatment Ask passes this value as one --jev-model argument.
            decisions_model(runtime.get("jev_model"))
        # runtime.template.json keeps every unselected mode field null; null and
        # absent both mean unselected, and any value is a mode conflict.
        mode = selected_mode(runtime)
        self.preflight["decisions"] = {"mode": mode}
        if mode == "preparation":
            decisions_control(runtime["decisions_preparation"])
            require(runtime["reader_mode"] == "rich_multimodal"
                    and runtime["jev_model"] == "openai/gpt-6-luna-decisions",
                    "decisions_preparation_mode_conflict")
        elif mode == "invocation_bounded":
            # No shared ledger or budget file: each legacy Ask keeps the product's
            # own single-invocation context, and a per-trial host claim allows one
            # Ask per trial. The cohort's bound must fit the cap.
            require(runtime["reader_mode"] == "legacy_text", "decisions_invocation_bounded_mode_conflict")
            decisions_directory(runtime.get("decisions_claims_dir"))
            self.preflight["decisions"] = invocation_cost_bound(runtime, len(self.rows))
        else:
            budget = runtime["decisions_budget"]
            require(type(budget) is dict and set(budget) == {"path", "bytes", "sha256"}
                    and Path(budget["path"]).is_absolute(), "decisions_budget_pin")
            integer(budget["bytes"], 1, 16384)
            digest(budget["sha256"])
            validate_asset_path(budget["path"])
            ledger_dir = runtime["decisions_ledger_dir"]
            require(isinstance(ledger_dir, str) and ledger_dir.startswith("/") and ledger_dir != "/" and
                    Path(ledger_dir).as_posix() == ledger_dir and ".." not in Path(ledger_dir).parts and
                    not any(c in ledger_dir for c in ("$", "\n", "\r", "\x00")), "ledger_directory_reference")
            # One shared allowance for all treatment trials: record it and warn.
            self.preflight["decisions"], warnings = shared_allowance(budget, len(self.rows))
            self.preflight["warnings"] += warnings
        require(runtime["account_scope"] == "Dev/Test/Eval" and runtime["admitted"] is True,
                "isolated_runtime_admission")
        # This receipt is independently selected by root. Source preparation does
        # not manufacture approval by setting these fields.
        approval = parse(pinned(runtime["admission"]))
        subject = {key: value for key, value in runtime.items() if key not in ("admission", "admitted")}
        require(approval.get("accepted") is True and approval.get("scope") == "harbor_full62_paired_once"
                and approval.get("run_id") == runtime["run_id"] and
                approval.get("runtime_subject_sha256") == sha(canonical(subject)) and
                approval.get("source_cohort_sha256") == self.cohort["source_cohort_sha256"] and
                approval.get("profile") == PROFILE, "admission_join")
        require(runtime["platform"] in ("linux/amd64", "linux/arm64"), "linux_binary_admission")
        require(approval.get("platform") == runtime["platform"] and
                approval.get("images") == runtime["images"] and
                approval.get("native_cli_contract_qualified") is True and
                approval.get("reader_base_binary") == runtime["reader_base_binary"] and
                approval.get("bare_image_excludes_gptgrep_and_indexes") is True and
                approval.get("raw_only_index_provenance_verified") is True,
                "platform_and_isolation_admission")
        require(approval.get("named_permissions_qualified") is True and
                approval.get("reader_effective_permissions_equal") is True and
                approval.get("native_cli") == runtime["native_cli"], "native_permissions_admission")
        # Runtime-only fact: the qualified readers and Judge expose no connector,
        # plugin, MCP or hosted web-search tool. Configuration alone cannot show it.
        require(approval.get("reader_external_tools_excluded") is True, "external_tools_admission")
        reader_pin = runtime["reader_base_binary"]
        require(set(reader_pin) == {"path", "bytes", "sha256"} and
                reader_pin["path"] == runtime["codex_binary"], "same_search_reader_binary")
        integer(reader_pin["bytes"], 1, 2**30)
        digest(reader_pin["sha256"])
        product_pin = runtime["gptgrep_binary_pin"]
        require(set(product_pin) == {"path", "bytes", "sha256"} and
                product_pin["path"] == runtime["gptgrep_binary"], "product_binary_pin")
        integer(product_pin["bytes"], 1, 2**31)
        digest(product_pin["sha256"])
        require(type(runtime["judge_profile"]) is dict and set(runtime["judge_profile"]) == set(PROFILE)
                and all(isinstance(value, str) and 0 < len(value) <= 128
                        for value in runtime["judge_profile"].values()), "frozen_judge_profile")
        require(set(runtime["permitted_tools"]) == set(ARMS) and
                all(isinstance(runtime["permitted_tools"][arm], list) and
                    0 < len(runtime["permitted_tools"][arm]) <= 64 and
                    all(isinstance(tool, str) and 0 < len(tool) <= 128
                        for tool in runtime["permitted_tools"][arm]) for arm in ARMS), "finite_permitted_tools")
        require(isinstance(runtime["generic_capabilities"], list) and
                isinstance(runtime["qualification_mismatches"], list), "qualification_observations")
        for role in ("gptgrep", "raw_only", "judge"):
            image(runtime["images"][role])
        base = runtime["base_tool_environment"]
        require(set(base) == {"image", "provenance"}, "common_base_environment")
        image(base["image"])
        require(runtime["images"]["raw_only"] == runtime["images"]["judge"] == base["image"],
                "shared_raw_judge_base")
        require(runtime["gptgrep_binary"] == PRODUCT_ENTRYPOINT, "product_entrypoint_path")
        bundle_identity(runtime.get("gptgrep_bundle"), runtime["platform"], product_pin, base["image"])
        tools = base_tools_identity(base["provenance"])
        require(tools["binaries"]["codex"] == runtime["reader_base_binary"] and
                tools["build_inputs"]["platform"] == runtime["platform"], "common_base_runtime_join")
        require(set(BASE_CAPABILITIES) <= set(runtime["generic_capabilities"]), "shared_generic_capabilities")
        require(runtime["images"]["raw_only"] != runtime["images"]["gptgrep"], "distinct_raw_image")
        for role in ("reader_env", "judge_env"):
            env_templates(runtime[role])
        require("OPENROUTER_API_KEY" not in runtime["judge_env"], "product_credential_in_judge")
        require(runtime["primary_metric"] == "answer_equivalence_delta", "selected_primary_metric")
        integer(runtime["bootstrap_seed"])
        integer(runtime["trial_order_seed"])
        require(re.fullmatch(r"[a-z0-9][a-z0-9-]{0,79}", runtime["run_id"]), "run_id")
        require(Path(runtime["jobs_dir"]).is_absolute(), "explicit_jobs_directory")
        if mode == "invocation_bounded":
            # Harbor deletes unfinished trial directories when it resumes a job;
            # the claims must survive that, so they never share jobs_dir.
            claims, jobs = Path(runtime["decisions_claims_dir"]), Path(runtime["jobs_dir"]).resolve()
            require(not claims.is_relative_to(jobs) and not jobs.is_relative_to(claims),
                    "decisions_claims_inside_jobs_dir")
        integer(runtime["max_asset_bytes"], 1, 2**40)
        for key in ("deadline_seconds", "judge_deadline_seconds"):
            integer(runtime[key], 1, 3600)
        self.preflight["tool_budget"], warnings = tool_budget(runtime["max_tool_calls"],
                                                              runtime["qualification_mismatches"])
        self.preflight["warnings"] += warnings
        integer(runtime["max_input_bytes"], 1024, 1048576)
        # Validate every selected row before run() can create either arm. The
        # broad private row parser is not the product question capacity.
        question_cap = min(MAX_READER_QUESTION_BYTES, runtime["max_input_bytes"])
        require(all(len(row["question"].encode("utf-8")) <= question_cap for row in self.rows),
                "selected_cohort_reader_question_bound")
        for key in ("codex_binary", "gptgrep_binary", "codex_home"):
            value = runtime[key]
            require(isinstance(value, str) and value.startswith("/") and ".." not in Path(value).parts,
                    "sandbox_absolute_path")
        judge_contract(self.judge)
        documents = runtime["documents"]
        require(len(documents) == 34 and len({d["document_id"] for d in documents}) == 34 and
                {d["document_id"] for d in documents} == {r["document_id"] for r in self.rows}, "document_join")
        names = []
        for row in documents + runtime["index_files"]:
            names.append(relative(row["relative_path"]))
            validate_asset_path(row["relative_path"])
            validate_asset_path(row["pin"]["path"])
            integer(row["pin"]["bytes"], 0, runtime["max_asset_bytes"])
            digest(row["pin"]["sha256"])
        require(len(names) == len(set(names)) and runtime["index_files"], "finite_assets_unique")
        documents_by_id = {row["document_id"]: row for row in documents}
        # The same cap must cover the document line and question before either
        # arm is materialized, using the reader's exact common-payload encoder.
        require(all(len(task_prompt({"reader_mode": runtime["reader_mode"],
                        "raw_document": CORPUS_ROOT + "/" + documents_by_id[row["document_id"]]["relative_path"]},
                       row["question"])) <= question_cap for row in self.rows),
                "selected_cohort_reader_task_bound")
        require(sum(r["pin"]["bytes"] for r in documents + runtime["index_files"]) <=
                integer(runtime["max_assets_total_bytes"], 1), "asset_total_bound")
        require(approval.get("assets_sha256") == sha(canonical(documents + runtime["index_files"])) and
                approval.get("judge_sha256") == self.manifest["judge_contract"]["sha256"], "assets_judge_admission")
        # Fail before any task exists rather than in every treatment Ask.
        self.preflight["index"] = index_identity(runtime)

    def run(self, output_dir, *, limit=None, task_ids=None, overwrite=False):
        # No destructive overwrite: an interrupted or consumed output is retained.
        require(not overwrite, "overwrite_forbidden_use_fresh_run")
        require(limit is None and task_ids is None, "complete_paired_cohort_required")
        root = Path(output_dir).absolute()
        require(root.resolve() == root and not root.exists(), "fresh_physical_output")
        # A runtime mount must not become a Docker build-context member. Do not
        # resolve/stat/open the credential path to establish this lexical rule.
        for key in ("reader_file", "judge_file"):
            auth_file = Path(self.runtime["auth"][key])
            require(not auth_file.is_relative_to(root), "auth_inside_task_output")
        # Verify every finite payload before creating any task. No discovery scan.
        # Copying streams 1 MiB blocks; no corpus-sized in-memory buffer.
        assets = self.runtime["documents"] + self.runtime["index_files"]
        for asset in assets:
            copy_asset(asset["pin"], self.runtime["max_asset_bytes"])
        # Docker Compose reads the reader seccomp profile from its host path when
        # it creates each reader container; recheck the approved bytes first.
        pinned(reader_seccomp_profile(self.runtime.get("reader_seccomp_profile")))
        require(not (root.parent / (root.name + ".export.json")).exists() and
                not (root.parent / (root.name + ".job.json")).exists(), "fresh_companion_outputs")
        preparation = self.runtime.get("decisions_preparation")
        if preparation is not None:
            for key in ("controls_directory", "financial_family_directory", "trial_state_directory"):
                require(not Path(preparation[key]).is_relative_to(root), "decisions_inside_task_output")
            put(Path(preparation["trial_state_directory"]) / "sole-writer.lock", b"")
        if selected_mode(self.runtime) == "invocation_bounded":
            claims = Path(self.runtime["decisions_claims_dir"])
            require(not claims.is_relative_to(root) and not root.is_relative_to(claims),
                    "decisions_inside_task_output")
            # One claims root per materialized cohort: a consumed root, whose
            # trials may already have spent, cannot be materialized again.
            require(not any(claims.iterdir()), "decisions_claims_not_fresh")
            put(claims / "cohort.json", {"schema": "gptgrep.harbor.decisions-invocation-claims.v1",
                                         "run_id": self.runtime["run_id"],
                                         "runtime_sha256": self.manifest["runtime"]["sha256"],
                                         "treatment_trials": len(self.rows)})
        root.mkdir(mode=0o700, parents=False)
        package = Path(__file__).parent
        exported = []
        for row in self.rows:
            document = next(d for d in self.runtime["documents"] if d["document_id"] == row["document_id"])
            for arm in ARMS:
                # Names derive from runtime identity; no benchmark ID appears in code.
                name = "q-" + sha(row["pair_id"].encode())[:24] + "-" + arm.replace("_", "-")
                task = root / name
                require(not task.exists(), "task_name_collision")
                binding = {"pair_id": row["pair_id"], "document_id": row["document_id"], "arm": arm,
                           "cohort_sha256": self.manifest["cohort"]["sha256"],
                           "runtime_sha256": self.manifest["runtime"]["sha256"],
                           "judge_sha256": self.manifest["judge_contract"]["sha256"],
                           "question_sha256": sha(row["question"].encode()),
                           "document_sha256": document["pin"]["sha256"]}
                contexts = self._task(task, package, row, document, binding)
                record = {"task_name": name, "binding": binding}
                if contexts is not None:
                    record["local_image_contexts"] = contexts
                exported.append(record)
        index = {"schema": "gptgrep.harbor.export.v1", "cohort": self.manifest["cohort"],
                 "runtime": self.manifest["runtime"], "judge_contract": self.manifest["judge_contract"],
                 "source_cohort_sha256": self.cohort["source_cohort_sha256"], "tasks": exported,
                 "expected_pairs": 62, "expected_documents": 34, "runtime_started": False,
                 "preflight": self.preflight}
        order = paired_order(exported, self.runtime["trial_order_seed"])
        job = {"job_name": self.runtime["run_id"], "jobs_dir": self.runtime["jobs_dir"],
               "n_attempts": 1, "n_concurrent_trials": 1, "retry": {"max_retries": 0},
               "datasets": [], "tasks": [{"path": str(root / name), "source": root.name}
                                          for name in order["task_names"]],
               "environment": {"type": "docker", "delete": True},
               "agents": [{"import_path": "gptgrep_harbor.agent:PairedNativeAgent",
                           "model_name": PROFILE["model"], "env": self.runtime["reader_env"]}],
               "verifier": {"env": self.runtime["judge_env"]}}
        job_path = root.parent / (root.name + ".job.json")
        job_bytes = canonical(job) + b"\n"
        put(job_path, job_bytes)
        index.update(trial_order=order, job={"path": str(job_path), "bytes": len(job_bytes), "sha256": sha(job_bytes)})
        # Keep controls outside dataset discovery and freeze the concrete order
        # before any result. Harbor otherwise sorts dataset task directories.
        put(root.parent / (root.name + ".export.json"), index)
        return index

    def _task(self, task, package, row, document, binding):
        runtime, arm = self.runtime, binding["arm"]
        # Every task materializes a reader role: refuse a missing pin before writing.
        reader_security = reader_security_opt(runtime.get("reader_seccomp_profile"))
        template = package / "task-template"
        # Templates are finite source members, not a task-data discovery surface.
        for name in ("solution/solve.sh", "tests/test.sh"):
            put(task / name, (template / name).read_bytes())
        put(task / "instruction.md", row["question"].encode())
        reader = {"binding": binding, "profile": PROFILE,
                  "base_tool_environment": {"image": runtime["base_tool_environment"]["image"],
                      "provenance": {**runtime["base_tool_environment"]["provenance"], "path": BASE_TOOLS_PATH}},
                  "native_cli": runtime["native_cli"],
                  "native_config_sha256": sha(model_config(PROFILE, "reader")),
                  "reader_base_binary": runtime["reader_base_binary"],
                  "gptgrep_binary_pin": runtime["gptgrep_binary_pin"],
                  "gptgrep_bundle": {"provenance": {**runtime["gptgrep_bundle"]["provenance"],
                                                      "path": PRODUCT_PROVENANCE}},
                  "permitted_tools": runtime["permitted_tools"][arm],
                  "generic_capabilities": runtime["generic_capabilities"],
                  "qualification_mismatches": runtime["qualification_mismatches"],
                  "budget_scope": {"query_deadline": "shared", "instruction_bytes": "shared_local_cap",
                      "reader_question_max_bytes": min(MAX_READER_QUESTION_BYTES, runtime["max_input_bytes"]),
                      "tool_calls": "treatment_enforced_raw_observed_only",
                      "treatment_max_tool_calls": runtime["max_tool_calls"], "raw_tool_call_cap": "none",
                      "treatment_tool_cap_parity": tool_cap_parity(runtime["max_tool_calls"]),
                      "total_provider_input": "not_enforced_by_raw_cli"},
                  **{key: runtime[key] for key in ("platform", "codex_binary", "gptgrep_binary", "codex_home",
                     "jev_model", "reader_mode", "native_ptc", "codex_permissions",
                     "deadline_seconds", "max_tool_calls", "max_input_bytes")},
                  "source_relative_path": document["relative_path"],
                  "raw_document": "/data/corpus/" + document["relative_path"]}
        mode = reader["decisions_mode"] = selected_mode(runtime)
        preparation = runtime.get("decisions_preparation")
        trial_state = None
        if mode == "existing_context":
            reader["decisions_budget"] = {**runtime["decisions_budget"], "path": DECISIONS_BUDGET}
        elif mode == "preparation" and arm == "gptgrep":
            trial_id = sha(canonical(binding))[:32]
            trial_state = Path(preparation["trial_state_directory"]) / trial_id
            trial_state.mkdir(mode=0o700)
            reader["decisions_preparation"] = {
                "seed": {**preparation["seed"], "path": CONTROL_ROOT + "/seed.json"},
                # The unchanged financial guard caps each identifier at 96 bytes.
                "run_id": runtime["run_id"][:63] + "-" + trial_id, "max_concurrent_batches": 1}
        elif mode == "invocation_bounded" and arm == "gptgrep":
            # One fresh claim directory per treatment trial, outside the job.
            trial_state = Path(runtime["decisions_claims_dir"]) / sha(canonical(binding))[:32]
            trial_state.mkdir(mode=0o700)
        put(task / "environment/reader.json", reader)
        # The frozen response schema enters only the Judge's private /tests area.
        schema_bytes = output_schema_bytes(self.judge["response_schema"])
        put(task / "tests" / PurePosixPath(JUDGE_OUTPUT_SCHEMA).name, schema_bytes)
        judge = {"binding": binding, "question": row["question"], "reference": row["reference"],
                 "base_tool_environment": reader["base_tool_environment"],
                 "answer_format": row["answer_format"], "contract": self.judge,
                 "output_schema": {"path": JUDGE_OUTPUT_SCHEMA, "bytes": len(schema_bytes),
                                   "sha256": sha(schema_bytes)},
                 "codex_binary": runtime["codex_binary"],
                 "reader_base_binary": runtime["reader_base_binary"],
                 "judge_profile": runtime["judge_profile"],
                 "native_cli": runtime["native_cli"], "codex_home": CODEX_HOME,
                 "native_config_sha256": sha(model_config(runtime["judge_profile"], "judge")),
                 "judge_deadline_seconds": runtime["judge_deadline_seconds"]}
        put(task / "tests/judge.json", judge)
        for target in (task / "environment/gptgrep_harbor", task / "tests/gptgrep_harbor"):
            for name in ("__init__.py", "common.py", "process.py", "native.py", "runtime.py", "product_bundle.py", "decisions.py", "reader.py", "verifier.py"):
                put(target / name, (package / name).read_bytes())
        for asset in runtime["documents"] + (runtime["index_files"] if arm == "gptgrep" else []):
            destination = task / "environment/corpus" / asset["relative_path"]
            # Recheck immediately before copying; never traverse unlisted inputs.
            copy_asset(asset["pin"], runtime["max_asset_bytes"], destination)
            destination.chmod(0o444)
        envdocker = ("FROM " + runtime["images"][arm] + "\n"
                     "COPY gptgrep_harbor /opt/full62/gptgrep_harbor\n"
                     "COPY reader.json /opt/full62/reader.json\n"
                     "COPY codex-native /opt/full62/codex-native\n"
                     "COPY corpus /data/corpus\n"
                     "COPY config.toml " + CODEX_HOME + "/config.toml\n"
                     "RUN mkdir -p " + WORK_DIR + " && chmod 700 " + CODEX_HOME + " " + WORK_DIR +
                     " && chmod 755 /opt/full62/codex-native\n"
                     "ENV PYTHONPATH=/opt/full62 PYTHONDONTWRITEBYTECODE=1 CODEX_HOME=" + CODEX_HOME + "\n"
                     "WORKDIR /data/corpus\n")
        judgedocker = ("FROM " + runtime["images"]["judge"] + "\n"
                       "COPY gptgrep_harbor /tests/gptgrep_harbor\n"
                       "COPY judge.json " + PurePosixPath(JUDGE_OUTPUT_SCHEMA).name + " test.sh /tests/\n"
                       "COPY config.toml " + CODEX_HOME + "/config.toml\n"
                       "RUN mkdir -p " + WORK_DIR + " && chmod 700 " + CODEX_HOME + " " + WORK_DIR + "\n"
                       "ENV PYTHONPATH=/tests PYTHONDONTWRITEBYTECODE=1 CODEX_HOME=" + CODEX_HOME + "\n"
                       "WORKDIR " + WORK_DIR + "\n")
        put(task / "environment/Dockerfile", envdocker.encode())
        put(task / "tests/Dockerfile", judgedocker.encode())
        put(task / "environment/codex-native", b"#!/usr/bin/env python3\nfrom gptgrep_harbor.runtime import native_wrapper\nnative_wrapper()\n")
        for directory, role, auth_key in (("environment", "reader", "reader_file"),
                                           ("tests", "judge", "judge_file")):
            profile = PROFILE if role == "reader" else runtime["judge_profile"]
            put(task / directory / "config.toml", model_config(profile, role))
            compose = auth_compose(runtime["auth"][auth_key], runtime["platform"], runtime["run_id"])
            if role == "reader":
                # Both arms identically. The Judge/verifier compose keeps
                # Docker's default profile and carries no security option.
                compose["services"]["main"]["security_opt"] = list(reader_security)
            if role == "reader" and arm == "gptgrep":
                if mode == "preparation":
                    mounts = [(preparation["controls_directory"], CONTROL_ROOT, True),
                              (preparation["financial_family_directory"], FAMILY_ROOT, False),
                              (str(trial_state), TRIAL_ROOT, False),
                              (str(Path(preparation["trial_state_directory"]) / "sole-writer.lock"), WRITER_LOCK,
                               False)]
                elif mode == "existing_context":
                    mounts = [(runtime["decisions_budget"]["path"], DECISIONS_BUDGET, True),
                              (runtime["decisions_ledger_dir"], DECISIONS_LEDGER_DIR, False)]
                else:
                    # invocation_bounded: no ledger or budget state leaves the
                    # host; only this trial's own claim directory is writable.
                    mounts = [(str(trial_state), CLAIM_ROOT, False)]
                for source, target, read_only in mounts:
                    compose["services"]["main"]["volumes"].append({"type": "bind",
                        "source": source, "target": target, "read_only": read_only,
                        "bind": {"create_host_path": False}})
            put(task / directory / "docker-compose.yaml", compose)
            # Credentials and arbitrary sibling files cannot enter either image.
            allowed = ["gptgrep_harbor", "config.toml"] + (
                ["reader.json", "codex-native", "corpus"] if role == "reader" else
                ["judge.json", PurePosixPath(JUDGE_OUTPUT_SCHEMA).name, "test.sh"])
            ignore = ["**"]
            for name in allowed:
                ignore.append("!" + name)
                if name in ("gptgrep_harbor", "corpus"):
                    ignore.append("!" + name + "/**")
            put(task / directory / ".dockerignore", ("\n".join(ignore) + "\n").encode())
        text = (template / "task.toml").read_text().format(
            task_name=task.name, reader_timeout=runtime["deadline_seconds"] + 10,
            judge_timeout=runtime["judge_deadline_seconds"] + 10,
            cpus=integer(runtime["resources"]["cpus"], 1, 64),
            memory_mb=integer(runtime["resources"]["memory_mb"], 256, 1048576),
            storage_mb=integer(runtime["resources"]["storage_mb"], 256, 1048576))
        put(task / "task.toml", text.encode())
        if any(value.startswith("sha256:") for value in runtime["images"].values()):
            from .local_images import task_contexts
            return task_contexts(task, {"environment": runtime["images"][arm],
                                        "tests": runtime["images"]["judge"]}, runtime["platform"])
        return None
