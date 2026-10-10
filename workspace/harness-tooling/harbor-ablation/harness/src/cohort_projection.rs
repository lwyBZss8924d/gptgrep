//! Runtime-only, complete-cohort projection. Never logs question or reference content.
use super::*;

const COHORT_SCHEMA: &str = "gptgrep.harbor.cohort.v2";
const MAPPING_SCHEMA: &str = "gptgrep.harbor.cohort-mapping.v2";
const DRAFT_SCHEMA: &str = "gptgrep.harbor.cohort-graders-draft.v1";
const MAX_COHORT_TASKS: usize = 1024;
const MAX_COHORT_POOLS: usize = 128;
const MAX_SOURCE_METADATA: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct SourceLock {
    benchmark: BenchmarkLock,
}
#[derive(Deserialize)]
struct BenchmarkLock {
    revision: String,
    files: BTreeMap<String, String>,
    pdf_sha256: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct SourceRow {
    doc_id: String,
    question: String,
    answer: Value,
    answer_format: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolInput {
    id: String,
    corpus: External,
    options: External,
    /// Original source document ID -> exact singleton sealed group ID.
    documents: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    schema_version: String,
    rubric_sha256: String,
    pools: Vec<PoolInput>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RosterRow {
    id: String,
    source_row: usize,
    row_sha256: String,
    question_sha256: String,
    source_id_sha256: String,
    raw_sha256: String,
    source_pool_id: String,
    batch_id: String,
    group_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cohort {
    schema_version: String,
    questions: External,
    sources_lock: External,
    mapping: External,
    source_revision: String,
    revision_basis: String,
    row_hash_encoding: String,
    denominator: usize,
    roster: Vec<RosterRow>,
    batches: Vec<BatchBinding>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchBinding {
    id: String,
    source_pool_id: String,
    corpus_sha256: String,
    options_sha256: String,
    readers_sha256: String,
    task_count: usize,
}
#[derive(Clone)]
struct Scope {
    pool_id: String,
    group_id: String,
    raw: Raw,
}
struct Rows {
    roster: Vec<RosterRow>,
    readers: BTreeMap<String, Vec<Value>>,
    grades: BTreeMap<String, Vec<Value>>,
    references: BTreeMap<String, Vec<u8>>,
}
struct Projection {
    files: BTreeMap<String, Vec<u8>>,
    cohort: Cohort,
    held: Vec<Arc<Held>>,
    corpora: Vec<BoundCorpus>,
}
struct BoundCorpus {
    id: String,
    input: External,
    groups: Vec<GroupBinding>,
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    // Serde diagnostics may contain an unexpected string supplied in private input.
    serde_json::from_slice(bytes).map_err(|_| "cohort_input_schema".into())
}
fn input(path: &Path, sha: &str) -> Result<Arc<Held>> {
    check_hash(sha)?;
    if !path.is_absolute() {
        return Err("cohort_input_absolute".into());
    }
    Held::open_limit(path, Some(sha), MAX_JSON)
}
fn external(held: &Held) -> External {
    External {
        path: held.path.clone(),
        sha256: held.sha.clone(),
    }
}
fn singleton(raw: &[Raw], source_id: &str, expected: &str) -> Result<Raw> {
    if raw.len() != 1 || raw[0].path != source_id || raw[0].sha256 != expected {
        return Err("cohort_scope_must_equal_source_document".into());
    }
    Ok(raw[0].clone())
}
fn compile_rows(
    values: &[Value],
    questions_sha: &str,
    scopes: &BTreeMap<String, Scope>,
    output: &Path,
) -> Result<Rows> {
    if values.is_empty() || values.len() > MAX_COHORT_TASKS {
        return Err("cohort_task_count".into());
    }
    let mut result = Rows {
        roster: Vec::new(),
        readers: BTreeMap::new(),
        grades: BTreeMap::new(),
        references: BTreeMap::new(),
    };
    let mut used = BTreeSet::new();
    for (ordinal, value) in values.iter().enumerate() {
        let row: SourceRow = decode(&json(value)?)?;
        check_rel(&row.doc_id)?;
        let scope = scopes.get(&row.doc_id).ok_or("cohort_source_not_mapped")?;
        used.insert(row.doc_id);
        if row.question.is_empty()
            || row.question.len() > 12000
            || row.answer.is_null()
            || row.answer_format.is_empty()
            || row.answer_format.len() > 4096
        {
            return Err("cohort_reference_contract".into());
        }
        let identity = hash(format!("{COHORT_SCHEMA}:{questions_sha}:{ordinal}").as_bytes());
        let id = format!("q-{}", &identity[..32]);
        let batch_id = format!("batch-{}", &identity[..32]);
        let question_sha = hash(row.question.as_bytes());
        let reference = json(&value!({
            "question": row.question, "answer": row.answer, "answer_format": row.answer_format
        }))?;
        if reference.len() > 65536 {
            return Err("cohort_reference_size".into());
        }
        let name = format!("private/references/{id}.json");
        let readers = result.readers.entry(batch_id.clone()).or_default();
        readers.push(value!({
            "id": id, "question": row.question, "question_sha256": question_sha,
            "group_scope": [scope.group_id]
        }));
        if readers.len() > 64 {
            return Err("cohort_batch_task_limit".into());
        }
        result
            .grades
            .entry(batch_id.clone())
            .or_default()
            .push(value!({
                "id": id, "question_sha256": question_sha,
                "reference_asset": {"path": output.join(&name), "sha256": hash(&reference)}
            }));
        if result.references.insert(name, reference).is_some() {
            return Err("cohort_identity_collision".into());
        }
        result.roster.push(RosterRow {
            id,
            source_row: ordinal,
            row_sha256: hash(&json(value)?),
            question_sha256: question_sha,
            source_id_sha256: hash(scope.raw.path.as_bytes()),
            raw_sha256: scope.raw.sha256.clone(),
            source_pool_id: scope.pool_id.clone(),
            batch_id,
            group_id: scope.group_id.clone(),
        });
    }
    if used != scopes.keys().cloned().collect() {
        return Err("cohort_mapping_must_cover_exact_source_set".into());
    }
    Ok(result)
}
fn add(files: &mut BTreeMap<String, Vec<u8>>, name: String, bytes: Vec<u8>) -> Result<()> {
    check_rel(&name)?;
    if bytes.len() as u64 > MAX_JSON || files.insert(name, bytes).is_some() {
        return Err("cohort_output_collision_or_json_limit".into());
    }
    if files.len() > MAX_FILES
        || files
            .values()
            .try_fold(0usize, |n, b| n.checked_add(b.len()))
            .ok_or("cohort_output_overflow")?
            > MAX_OUTPUT
    {
        return Err("cohort_output_limit".into());
    }
    Ok(())
}
fn build(
    questions: Arc<Held>,
    lock: Arc<Held>,
    mapping: Arc<Held>,
    output: &Path,
) -> Result<Projection> {
    let source: SourceLock = decode(&lock.bytes()?)?;
    let map: Mapping = decode(&mapping.bytes()?)?;
    let benchmark = source.benchmark;
    if benchmark.files.get("questions.json") != Some(&questions.sha)
        || benchmark.revision.len() != 40
        || !benchmark.revision.bytes().all(|c| c.is_ascii_hexdigit())
        || map.schema_version != MAPPING_SCHEMA
        || map.pools.is_empty()
        || map.pools.len() > MAX_COHORT_POOLS
    {
        return Err("cohort_source_or_mapping_binding".into());
    }
    check_hash(&map.rubric_sha256)?;
    for (name, sha) in &benchmark.pdf_sha256 {
        check_rel(name)?;
        check_hash(sha)?;
    }
    let mut held = vec![questions.clone(), lock.clone(), mapping.clone()];
    let values: Vec<Value> = decode(&questions.bytes()?)?;
    if values.is_empty() || values.len() > MAX_COHORT_TASKS {
        return Err("cohort_task_count".into());
    }
    let mut corpora = Vec::new();
    let mut metadata_bytes = 0usize;
    let mut scopes = BTreeMap::new();
    let mut batches = BTreeSet::new();
    for batch in &map.pools {
        check_id(&batch.id)?;
        if !batches.insert(batch.id.clone()) {
            return Err("cohort_duplicate_batch".into());
        }
        let corpus_file = input(&batch.corpus.path, &batch.corpus.sha256)?;
        let option_file = input(&batch.options.path, &batch.options.sha256)?;
        options(&option_file.bytes()?)?;
        let corpus = admit_held(corpus_file.clone(), &corpus_file.bytes()?)?;
        let mut groups = BTreeSet::new();
        for (source_id, group_id) in &batch.documents {
            check_rel(source_id)?;
            check_id(group_id)?;
            let expected = benchmark
                .pdf_sha256
                .get(source_id)
                .ok_or("cohort_source_not_locked")?;
            let group = corpus
                .groups
                .iter()
                .find(|g| &g.binding.id == group_id)
                .ok_or("cohort_group_missing")?;
            if !groups.insert(group_id.clone()) {
                return Err("cohort_group_mapping_ambiguous".into());
            }
            let raw = singleton(&group.binding.raw_documents, source_id, expected)?;
            if scopes
                .insert(
                    source_id.clone(),
                    Scope {
                        pool_id: batch.id.clone(),
                        group_id: group_id.clone(),
                        raw,
                    },
                )
                .is_some()
            {
                return Err("cohort_duplicate_source_mapping".into());
            }
        }
        if groups.is_empty() || groups.len() != corpus.groups.len() {
            return Err("cohort_corpus_group_set".into());
        }
        let groups = corpus
            .groups
            .iter()
            .map(|g| g.binding.clone())
            .collect::<Vec<_>>();
        metadata_bytes = metadata_bytes
            .checked_add(json(&groups)?.len())
            .filter(|n| *n <= MAX_SOURCE_METADATA)
            .ok_or("cohort_source_metadata_limit")?;
        corpora.push(BoundCorpus {
            id: batch.id.clone(),
            input: external(&corpus_file),
            groups,
        });
        held.extend([corpus_file, option_file]);
        // Drop each admitted corpus before the next batch: never retain all native readers/FDs.
    }
    let rows = compile_rows(&values, &questions.sha, &scopes, output)?;
    let cohort = Cohort {
        schema_version: COHORT_SCHEMA.into(),
        questions: external(&questions),
        sources_lock: external(&lock),
        mapping: external(&mapping),
        source_revision: benchmark.revision,
        revision_basis: "pinned_source_lock_not_git_checkout_observation".into(),
        row_hash_encoding: "serde_json_pretty_v1".into(),
        denominator: values.len(),
        roster: Vec::new(),
        batches: Vec::new(),
    };
    let (cohort, files) = projected_files(rows, &map, cohort)?;
    Ok(Projection {
        files,
        cohort,
        held,
        corpora,
    })
}
fn projected_files(
    rows: Rows,
    map: &Mapping,
    mut cohort: Cohort,
) -> Result<(Cohort, BTreeMap<String, Vec<u8>>)> {
    let mut files = rows.references;
    let mut bindings = Vec::new();
    for row in &rows.roster {
        let pool = map
            .pools
            .iter()
            .find(|p| p.id == row.source_pool_id)
            .ok_or("cohort_source_pool_missing")?;
        let tasks = rows
            .readers
            .get(&row.batch_id)
            .ok_or("cohort_empty_batch")?;
        if tasks.len() != 1 {
            return Err("cohort_ordinal_shard_shape".into());
        }
        let bytes = json(&value!({
            "schema_version": "gptgrep.harbor.production-readers.v1",
            "corpus_sha256": pool.corpus.sha256, "tasks": tasks
        }))?;
        bindings.push(BatchBinding {
            id: row.batch_id.clone(),
            source_pool_id: pool.id.clone(),
            corpus_sha256: pool.corpus.sha256.clone(),
            options_sha256: pool.options.sha256.clone(),
            readers_sha256: hash(&bytes),
            task_count: tasks.len(),
        });
        add(&mut files, format!("readers/{}.json", row.batch_id), bytes)?;
    }
    cohort.roster = rows.roster;
    cohort.batches = bindings;
    let cohort_bytes = json(&cohort)?;
    let cohort_sha = hash(&cohort_bytes);
    add(&mut files, "cohort.json".into(), cohort_bytes)?;
    for batch in &cohort.batches {
        add(
            &mut files,
            format!("private/{}.graders-draft.json", batch.id),
            json(&value!({
                "schema_version": DRAFT_SCHEMA, "cohort_sha256": cohort_sha,
                "batch_id": batch.id, "corpus_sha256": batch.corpus_sha256,
                "reader_projection_sha256": batch.readers_sha256, "options_sha256": batch.options_sha256,
                "rubric_sha256": map.rubric_sha256, "judge_status": "unavailable",
                "tasks": rows.grades[&batch.id]
            }))?,
        )?;
    }
    let completion = json(&value!({
        "schema_version": "gptgrep.harbor.cohort-projection-complete.v1",
        "cohort_sha256": cohort_sha, "denominator": cohort.denominator,
        "files": files.iter().map(|(name, bytes)| (name.clone(), hash(bytes))).collect::<BTreeMap<_, _>>()
    }))?;
    add(&mut files, "projection-complete.json".into(), completion)?;
    Ok((cohort, files))
}

impl Projection {
    fn fresh(&self) -> Result<()> {
        for held in &self.held {
            held.fresh()?;
        }
        for corpus in &self.corpora {
            let file = input(&corpus.input.path, &corpus.input.sha256)?;
            let admitted = admit_held(file.clone(), &file.bytes()?)?;
            let groups = admitted
                .groups
                .iter()
                .map(|g| &g.binding)
                .collect::<Vec<_>>();
            if json(&groups)? != json(&corpus.groups)? {
                return Err("cohort_source_group_changed".into());
            }
            admitted.fresh()?;
        }
        Ok(())
    }
    fn publish(&self, output: &Path) -> Result<()> {
        canonical_root(output.parent().ok_or("cohort_output_parent")?)?;
        if !output.is_absolute()
            || output.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("cohort_output_canonical".into());
        }
        if self.held.iter().any(|h| h.path.starts_with(output))
            || self
                .corpora
                .iter()
                .flat_map(|c| &c.groups)
                .any(|g| output.starts_with(&g.canonical_root))
        {
            return Err("cohort_output_input_overlap".into());
        }
        self.fresh()?;
        let writer = PrivateOutput::new(output)?;
        for (name, bytes) in &self.files {
            if name == "projection-complete.json" {
                continue;
            }
            let mut file = writer.file(name)?;
            file.write_all(bytes).map_err(err)?;
            file.sync_all().map_err(err)?;
        }
        self.fresh()?;
        let expected_names = self
            .files
            .keys()
            .filter(|n| n.as_str() != "projection-complete.json")
            .cloned()
            .collect();
        if scan(output, false)? != expected_names {
            return Err("cohort_output_set".into());
        }
        for (name, bytes) in &self.files {
            if name != "projection-complete.json" {
                Held::open_limit(&output.join(name), Some(&hash(bytes)), MAX_JSON)?;
            }
        }
        let mut done = writer.file("projection-complete.json")?;
        done.write_all(&self.files["projection-complete.json"])
            .map_err(err)?;
        done.sync_all().map_err(err)?;
        writer.root.sync_all().map_err(err)
    }
}

pub(crate) fn project(
    questions: &Path,
    questions_sha: &str,
    lock: &Path,
    lock_sha: &str,
    mapping: &Path,
    mapping_sha: &str,
    output: &Path,
) -> Result<()> {
    let projection = build(
        input(questions, questions_sha)?,
        input(lock, lock_sha)?,
        input(mapping, mapping_sha)?,
        output,
    )?;
    projection.publish(output)
}

fn reader_binding(
    manifest: &Manifest,
    cohort: &Cohort,
    batch: &BatchBinding,
    groups: &[&GroupBinding],
) -> Result<()> {
    let roster: Vec<_> = cohort
        .roster
        .iter()
        .filter(|r| r.batch_id == batch.id)
        .collect();
    if manifest.role != "reader"
        || manifest.corpus_sha256 != batch.corpus_sha256
        || manifest.reader_projection_sha256 != batch.readers_sha256
        || manifest.options_sha256 != batch.options_sha256
        || manifest.task_bindings.len() != roster.len()
        || json(&manifest.group_bindings)? != json(&groups)?
    {
        return Err("cohort_reader_export_binding".into());
    }
    for (task, row) in manifest.task_bindings.iter().zip(roster) {
        if task.id != row.id
            || task.question_sha256 != row.question_sha256
            || task.group_scope != [row.group_id.clone()]
            || task.helper.is_some()
            || task.raw_sha256.len() != 1
            || task.raw_sha256.values().next() != Some(&row.raw_sha256)
            || task
                .raw_sha256
                .keys()
                .next()
                .and_then(|key| key.strip_prefix(&format!("{}/", row.group_id)))
                .is_none_or(|path| hash(path.as_bytes()) != row.source_id_sha256)
        {
            return Err("cohort_reader_task_binding".into());
        }
    }
    Ok(())
}

fn frozen_projection(root: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<Arc<Held>>> {
    if scan(root, false)? != files.keys().cloned().collect() {
        return Err("cohort_frozen_output_set".into());
    }
    files
        .iter()
        .map(|(name, bytes)| input(&root.join(name), &hash(bytes)))
        .collect()
}

pub(crate) fn bind(
    cohort_path: &Path,
    cohort_sha: &str,
    batch_id: &str,
    reader_export: &Path,
    reader_export_sha: &str,
    output: &Path,
) -> Result<()> {
    check_id(batch_id)?;
    check_hash(reader_export_sha)?;
    let cohort_file = input(cohort_path, cohort_sha)?;
    let cohort: Cohort = decode(&cohort_file.bytes()?)?;
    let root = cohort_path.parent().ok_or("cohort_root")?;
    canonical_root(root)?;
    if output.starts_with(root)
        || output.starts_with(reader_export)
        || reader_export.starts_with(output)
    {
        return Err("cohort_bound_output_overlap".into());
    }
    // Regenerate every row and draft from the same pinned sources, not a selected subset.
    let mut expected = build(
        input(&cohort.questions.path, &cohort.questions.sha256)?,
        input(&cohort.sources_lock.path, &cohort.sources_lock.sha256)?,
        input(&cohort.mapping.path, &cohort.mapping.sha256)?,
        root,
    )?;
    if hash(&expected.files["cohort.json"]) != cohort_sha {
        return Err("cohort_full_roster_changed".into());
    }
    expected
        .held
        .extend(frozen_projection(root, &expected.files)?);
    let batch = expected
        .cohort
        .batches
        .iter()
        .find(|b| b.id == batch_id)
        .ok_or("cohort_batch_missing")?;
    let exported = input(
        &reader_export.join("export-manifest.json"),
        reader_export_sha,
    )?;
    let (manifest, export_sha) = validate(reader_export)?;
    if export_sha != reader_export_sha {
        return Err("cohort_export_digest".into());
    }
    let pool = expected
        .corpora
        .iter()
        .find(|p| p.id == batch.source_pool_id)
        .ok_or("cohort_source_pool_missing")?;
    let groups = pool.groups.iter().collect::<Vec<_>>();
    reader_binding(&manifest, &expected.cohort, batch, &groups)?;
    expected.held.push(exported);
    let mut graders: Value =
        decode(&expected.files[&format!("private/{batch_id}.graders-draft.json")])?;
    let object = graders.as_object_mut().ok_or("cohort_grader_draft")?;
    for field in ["cohort_sha256", "batch_id", "options_sha256"] {
        object.remove(field);
    }
    object.insert(
        "schema_version".into(),
        value!("gptgrep.harbor.production-verifiers.v1"),
    );
    object.insert("reader_export_sha256".into(), value!(reader_export_sha));
    // Emit a new bound file; the original full roster and grader draft stay immutable.
    expected.files.clear();
    let bytes = json(&graders)?;
    let graders_sha = hash(&bytes);
    add(&mut expected.files, "graders.json".into(), bytes)?;
    add(
        &mut expected.files,
        "projection-complete.json".into(),
        json(&value!({
            "schema_version": "gptgrep.harbor.cohort-verifiers-bound.v1",
            "cohort_sha256": cohort_sha, "denominator": expected.cohort.denominator,
            "batch_id": batch_id, "reader_export_sha256": reader_export_sha,
            "graders_sha256": graders_sha
        }))?,
    )?;
    expected.publish(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> BTreeMap<String, Scope> {
        BTreeMap::from([(
            "source.pdf".into(),
            Scope {
                pool_id: "pool-a".into(),
                group_id: "group-a".into(),
                raw: Raw {
                    path: "source.pdf".into(),
                    bytes: 100,
                    sha256: "a".repeat(64),
                },
            },
        )])
    }
    fn source(question: &str) -> Value {
        value!({"doc_id":"source.pdf", "question": question, "answer":"PRIVATE_SENTINEL",
            "answer_format":"text", "evidence_pages":"PRIVATE_EVIDENCE"})
    }
    fn source_pin(name: &str) -> External {
        External {
            path: Path::new("/inputs").join(name),
            sha256: hash(name.as_bytes()),
        }
    }
    fn assemble(
        values: &[Value],
        scopes: &BTreeMap<String, Scope>,
    ) -> (Cohort, BTreeMap<String, Vec<u8>>) {
        let rows = compile_rows(values, &"b".repeat(64), scopes, Path::new("/projection")).unwrap();
        let mut pools: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for (name, scope) in scopes {
            pools
                .entry(scope.pool_id.clone())
                .or_default()
                .insert(name.clone(), scope.group_id.clone());
        }
        let map = Mapping {
            schema_version: MAPPING_SCHEMA.into(),
            rubric_sha256: "d".repeat(64),
            pools: pools
                .into_iter()
                .map(|(id, documents)| PoolInput {
                    corpus: source_pin(&format!("{id}-corpus.json")),
                    options: source_pin(&format!("{id}-options.json")),
                    id,
                    documents,
                })
                .collect(),
        };
        let cohort = Cohort {
            schema_version: COHORT_SCHEMA.into(),
            questions: source_pin("questions.json"),
            sources_lock: source_pin("lock.json"),
            mapping: source_pin("mapping.json"),
            source_revision: "0".repeat(40),
            revision_basis: "test".into(),
            row_hash_encoding: "serde_json_pretty_v1".into(),
            denominator: values.len(),
            roster: Vec::new(),
            batches: Vec::new(),
        };
        projected_files(rows, &map, cohort).unwrap()
    }
    #[test]
    fn complete_roster_keeps_duplicate_questions_and_excludes_gold_from_readers() {
        let input = [source("Synthetic question"), source("Synthetic question")];
        let rows =
            compile_rows(&input, &"b".repeat(64), &scope(), Path::new("/projection")).unwrap();
        assert_eq!(rows.roster.len(), input.len());
        assert_ne!(rows.roster[0].id, rows.roster[1].id);
        assert_eq!(rows.roster[1].source_row, 1);
        let readers = String::from_utf8(json(&rows.readers).unwrap()).unwrap();
        assert!(!readers.contains("PRIVATE_SENTINEL"));
        assert!(!readers.contains("PRIVATE_EVIDENCE"));
        assert_eq!(rows.references.len(), input.len());
        assert!(
            rows.references
                .values()
                .all(|b| String::from_utf8_lossy(b).contains("PRIVATE_SENTINEL"))
        );
    }
    #[test]
    fn complete_mapping_rejects_missing_extra_and_overfull_task_sets() {
        let input = [source("Synthetic question")];
        assert!(
            compile_rows(
                &input,
                &"b".repeat(64),
                &BTreeMap::new(),
                Path::new("/projection")
            )
            .is_err()
        );
        let mut extra = scope();
        extra.insert("unused.pdf".into(), extra["source.pdf"].clone());
        assert!(compile_rows(&input, &"b".repeat(64), &extra, Path::new("/projection")).is_err());
        let too_many = vec![source("Synthetic question"); MAX_COHORT_TASKS + 1];
        assert!(
            compile_rows(
                &too_many,
                &"b".repeat(64),
                &scope(),
                Path::new("/projection")
            )
            .is_err()
        );
    }
    #[test]
    fn ordinal_shards_repeat_one_source_without_batch_coupling_or_lost_rows() {
        let values = vec![source("Repeated synthetic question"); 65];
        let (cohort, files) = assemble(&values, &scope());
        assert_eq!(cohort.denominator, values.len());
        assert_eq!(cohort.batches.len(), values.len());
        assert!(cohort.batches.iter().all(|batch| batch.task_count == 1));
        assert!(
            cohort
                .roster
                .iter()
                .all(|row| row.source_pool_id == "pool-a")
        );
        assert_eq!(
            cohort
                .roster
                .iter()
                .map(|row| &row.batch_id)
                .collect::<BTreeSet<_>>()
                .len(),
            values.len()
        );
        let mut seen = BTreeSet::new();
        for (ordinal, batch) in cohort.batches.iter().enumerate() {
            assert_eq!(cohort.roster[ordinal].source_row, ordinal);
            assert_eq!(cohort.roster[ordinal].batch_id, batch.id);
            let reader: Value = decode(&files[&format!("readers/{}.json", batch.id)]).unwrap();
            let tasks = reader["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0]["group_scope"], value!(["group-a"]));
            assert!(seen.insert(tasks[0]["id"].as_str().unwrap().to_owned()));
        }
        assert_eq!(
            seen,
            cohort.roster.iter().map(|row| row.id.clone()).collect()
        );
    }
    #[test]
    fn singleton_mapping_rejects_broader_group_and_changed_source() {
        let raw = scope()["source.pdf"].raw.clone();
        assert!(singleton(std::slice::from_ref(&raw), "source.pdf", &"a".repeat(64)).is_ok());
        assert!(singleton(&[raw.clone(), raw.clone()], "source.pdf", &"a".repeat(64)).is_err());
        assert!(singleton(std::slice::from_ref(&raw), "other.pdf", &"a".repeat(64)).is_err());
        assert!(singleton(&[raw], "source.pdf", &"b".repeat(64)).is_err());
    }
    #[test]
    fn all_rows_partition_once_and_gold_stays_out_of_reader_files_and_manifests() {
        let mut scopes = scope();
        let mut second = scopes["source.pdf"].clone();
        second.pool_id = "pool-b".into();
        second.group_id = "group-b".into();
        second.raw.path = "second.pdf".into();
        scopes.insert("second.pdf".into(), second);
        let mut other = source("Another synthetic question");
        other["doc_id"] = value!("second.pdf");
        let values = [
            source("A synthetic question"),
            other,
            source("A synthetic question"),
        ];
        let (cohort, files) = assemble(&values, &scopes);
        assert_eq!(cohort.denominator, values.len());
        assert_eq!(
            cohort
                .roster
                .iter()
                .map(|r| r.source_row)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let mut exported = BTreeSet::new();
        for (name, bytes) in &files {
            if name.starts_with("readers/") {
                let readers: Value = decode(bytes).unwrap();
                for row in readers["tasks"].as_array().unwrap() {
                    assert!(exported.insert(row["id"].as_str().unwrap().to_owned()));
                }
            }
            if !name.starts_with("private/") {
                let text = String::from_utf8_lossy(bytes);
                assert!(!text.contains("PRIVATE_SENTINEL"), "{name}");
                assert!(!text.contains("PRIVATE_EVIDENCE"), "{name}");
                assert!(!text.contains("answer_format"), "{name}");
            }
        }
        assert_eq!(
            exported,
            cohort.roster.iter().map(|r| r.id.clone()).collect()
        );
        let references = files
            .iter()
            .filter(|(n, _)| n.starts_with("private/references/"))
            .collect::<Vec<_>>();
        assert_eq!(references.len(), values.len());
        assert!(
            references
                .iter()
                .all(|(_, b)| String::from_utf8_lossy(b).contains("PRIVATE_SENTINEL"))
        );
    }
    #[test]
    fn bind_rejects_swapped_export_config_question_path_and_generation() {
        let (cohort, _) = assemble(&[source("Synthetic question")], &scope());
        let row = &cohort.roster[0];
        let batch = &cohort.batches[0];
        let group: GroupBinding = decode(&json(&value!({
            "id":"group-a", "canonical_root":"/raw", "runtime_root":"/raw",
            "raw_manifest_sha256":"a".repeat(64), "controls_sha256":"a".repeat(64),
            "seal_sha256":"a".repeat(64), "current_snapshot_sha256":"a".repeat(64),
            "pin":{"generation":"0".repeat(32), "manifest_sha256":"a".repeat(64)},
            "raw_documents":[{"path":"source.pdf", "bytes":100, "sha256":"a".repeat(64)}],
            "generation_inventory":[], "generation_bytes":0, "native_coverage":{},
            "index_engine_revision":"test", "index_file_table_id":"test", "index_manifest_sha256":"a".repeat(64)
        })).unwrap()).unwrap();
        let manifest = Manifest {
            schema_version: EXPORT_SCHEMA.into(),
            role: "reader".into(),
            corpus_sha256: batch.corpus_sha256.clone(),
            reader_projection_sha256: batch.readers_sha256.clone(),
            reader_export_sha256: None,
            options_sha256: batch.options_sha256.clone(),
            source_mode: "raw_only_native_v2_structure".into(),
            executable: false,
            qualification: qualification(),
            group_bindings: vec![group.clone()],
            task_bindings: vec![TaskBinding {
                id: row.id.clone(),
                question_sha256: row.question_sha256.clone(),
                group_scope: vec![row.group_id.clone()],
                raw_sha256: BTreeMap::from([("group-a/source.pdf".into(), "a".repeat(64))]),
                helper: None,
            }],
            files: BTreeMap::new(),
        };
        assert!(reader_binding(&manifest, &cohort, batch, &[&group]).is_ok());
        for change in 0..6 {
            let mut changed: Manifest = decode(&json(&manifest).unwrap()).unwrap();
            match change {
                0 => changed.corpus_sha256 = "e".repeat(64),
                1 => changed.reader_projection_sha256 = "e".repeat(64),
                2 => changed.options_sha256 = "e".repeat(64),
                3 => changed.task_bindings[0].question_sha256 = "e".repeat(64),
                4 => {
                    changed.task_bindings[0].raw_sha256 =
                        BTreeMap::from([("group-a/other.pdf".into(), "a".repeat(64))])
                }
                _ => changed.group_bindings[0].current_snapshot_sha256 = "e".repeat(64),
            }
            assert!(reader_binding(&changed, &cohort, batch, &[&group]).is_err());
        }
    }
    #[test]
    fn bind_rejects_changed_private_draft_and_unlisted_projection_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let files = BTreeMap::from([("draft.json".into(), b"original private draft".to_vec())]);
        fs::write(root.join("draft.json"), &files["draft.json"]).unwrap();
        assert!(frozen_projection(&root, &files).is_ok());
        fs::write(root.join("draft.json"), b"swapped private draft").unwrap();
        assert!(frozen_projection(&root, &files).is_err());
        fs::write(root.join("draft.json"), &files["draft.json"]).unwrap();
        fs::write(root.join("unlisted.json"), b"not admitted").unwrap();
        assert!(frozen_projection(&root, &files).is_err());
    }
}
