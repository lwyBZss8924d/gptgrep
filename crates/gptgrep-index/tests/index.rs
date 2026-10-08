use std::fs;
use std::path::{Path, PathBuf};

use gptgrep_index::{
    IndexContentLimits, IndexSession, Match, build, search, search_with_document,
    search_with_document_and_predicate,
};
use tempfile::TempDir;

fn fixture(files: &[(&str, &str)]) -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("text");
    let index = temp.path().join("trigram");
    fs::create_dir(&root).unwrap();
    for (name, value) in files {
        let destination = root.join(name);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, value).unwrap();
    }
    build(&root, &index).unwrap();
    (temp, root, index)
}

fn content_limits() -> IndexContentLimits {
    IndexContentLimits {
        max_files: 32,
        max_control_bytes: 1_048_576,
        max_index_bytes: 8_388_608,
        max_file_bytes: 65_536,
        max_total_text_bytes: 262_144,
        max_trigrams: 100_000,
        max_postings: 200_000,
        max_work_bytes: 33_554_432,
    }
}

fn refresh_trigram_count(index: &Path) {
    let path = index.join("meta.json");
    let mut meta: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    meta["num_trigrams"] = (fs::metadata(index.join("lookup.bin")).unwrap().len() / 16).into();
    fs::write(path, serde_json::to_vec(&meta).unwrap()).unwrap();
}

#[test]
fn content_verification_matches_actual_bom_unicode_multifile_and_short_indexes() {
    let inputs = [
        ("a.txt", "\u{feff}Copper WIRE\n中国文档\n"),
        ("nested/b.txt", "copper wire\nKEY and ſymbol\n"),
        ("c.txt", "ab"),
        ("d.txt", ""),
    ];
    let (_temp, _root, index) = fixture(&inputs);
    let report = IndexSession::verify_content(&index, &content_limits()).unwrap();
    assert_eq!(report.files, 4);
    assert_eq!(
        report.text_bytes,
        inputs.iter().map(|(_, s)| s.len() as u64).sum::<u64>()
    );
    assert!(report.trigrams > 0 && report.postings >= report.trigrams);
    assert_eq!(
        search(&index, "copper", true, true, 10).unwrap().hits.len(),
        2
    );
    assert_eq!(search(&index, "key", true, true, 10).unwrap().hits.len(), 1);
}

#[test]
fn content_verification_rejects_genuine_same_path_foreign_engine_despite_original_metadata() {
    let (_temp, _root, index) = fixture(&[("a.txt", "ordinary copper needle\n")]);
    let (_foreign, _foreign_root, foreign) = fixture(&[("a.txt", "different silver thread\n")]);
    IndexSession::verify_content(&index, &content_limits()).unwrap();
    assert_eq!(
        search(&index, "copper", false, true, 10)
            .unwrap()
            .hits
            .len(),
        1
    );
    // Real separately built foreign postings, with all original path/file/source
    // metadata retained and only the declared trigram count refreshed.
    for name in ["lookup.bin", "index.bin"] {
        fs::copy(foreign.join(name), index.join(name)).unwrap();
    }
    refresh_trigram_count(&index);
    let unchecked = IndexSession::open(&index).unwrap();
    assert!(
        unchecked
            .search("copper", false, true, 10)
            .unwrap()
            .hits
            .is_empty()
    );
    let error = IndexSession::verify_content(&index, &content_limits()).unwrap_err();
    assert!(error.to_string().contains("index content"));
}

#[test]
fn content_verification_rejects_duplicate_path_engine_omitting_a_manifest_source() {
    let (_temp, _root, index) = fixture(&[
        ("a.txt", "genuine copper conductor\n"),
        ("b.txt", "distinct silver document\n"),
    ]);
    let (_foreign, _foreign_root, foreign) = fixture(&[
        ("a.txt", "genuine copper conductor\n"),
        ("b.txt", "genuine copper conductor\n"),
    ]);
    IndexSession::verify_content(&index, &content_limits()).unwrap();
    assert_eq!(
        search(&index, "silver", false, true, 10)
            .unwrap()
            .hits
            .len(),
        1
    );
    // Both IDs' postings come from a real builder over A's text. The graft
    // retains the selected A/B text and manifest, but maps both dense IDs to A.
    for name in ["lookup.bin", "index.bin"] {
        fs::copy(foreign.join(name), index.join(name)).unwrap();
    }
    let mut table = fs::read(index.join("files.bin")).unwrap();
    let mut pos = 16;
    while pos < table.len() {
        let len = u16::from_le_bytes(table[pos + 4..pos + 6].try_into().unwrap()) as usize;
        pos += 6;
        assert_eq!(len, "a.txt".len());
        table[pos..pos + len].copy_from_slice(b"a.txt");
        pos += len;
    }
    fs::write(index.join("files.bin"), &table).unwrap();
    let table_id = tgrep_core::meta::file_table_id(&table);
    let mut meta = tgrep_core::meta::IndexMeta::load(&index).unwrap();
    meta.file_table_id = Some(table_id);
    meta.num_trigrams = fs::metadata(index.join("lookup.bin")).unwrap().len() / 16;
    meta.save(&index).unwrap();
    tgrep_core::path_index::write_extra_paths_with_visibility(
        &index,
        &[],
        &meta.visibility,
        table_id,
        meta.hidden_complete,
    )
    .unwrap();
    let manifest_path = index.join("gptgrep-index.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["file_table_id"] = table_id
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .into();
    fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let raw = tgrep_core::reader::IndexReader::open(&index).unwrap();
    assert_eq!(raw.all_paths(), &["a.txt", "a.txt"]);
    let plan = tgrep_core::query::build_query_plan("silver", false).unwrap();
    let omitted = tgrep_core::query::execute_plan_with_masks(&plan, &|tri| {
        raw.lookup_trigram_with_masks(tri)
    });
    assert!(
        omitted.is_empty(),
        "genuine foreign engine omits selected B's match"
    );
    match IndexSession::verify_content(&index, &content_limits()) {
        Ok(report) => panic!(
            "duplicate-path engine admitted: files={}, postings={}, omitted_source_candidates={}",
            report.files,
            report.postings,
            omitted.len()
        ),
        Err(error) => assert!(error.to_string().contains("source set")),
    }
    assert!(IndexSession::open(&index).is_err());
}

#[test]
fn content_verification_rejects_location_and_next_mask_edits() {
    for field in [4, 5] {
        let (_temp, _root, index) = fixture(&[("a.txt", "ordinary copper wire\n")]);
        let path = index.join("index.bin");
        let mut postings = fs::read(&path).unwrap();
        postings[field] ^= 1;
        if field == 4 && postings[field] == 0 {
            postings[field] = 2;
        }
        fs::write(path, postings).unwrap();
        IndexSession::open(&index).unwrap();
        assert!(
            IndexSession::verify_content(&index, &content_limits())
                .unwrap_err()
                .to_string()
                .contains("mask mismatch"),
            "field {field}"
        );
    }
}

#[test]
fn content_verification_rejects_extra_missing_and_duplicate_postings() {
    for change in ["extra", "missing", "duplicate", "invalid_id"] {
        let (_temp, _root, index) = fixture(&[("a.txt", "ordinary copper wire\n")]);
        let mut lookup = fs::read(index.join("lookup.bin")).unwrap();
        let mut postings = fs::read(index.join("index.bin")).unwrap();
        match change {
            "extra" => {
                lookup.extend_from_slice(&0x00ff_ffff_u32.to_le_bytes());
                lookup.extend_from_slice(&(postings.len() as u64).to_le_bytes());
                lookup.extend_from_slice(&1u32.to_le_bytes());
                postings.extend_from_slice(&[0, 0, 0, 0, 1, 0]);
            }
            "missing" => {
                lookup.drain(..16);
                postings.drain(..6);
                for row in lookup.chunks_exact_mut(16) {
                    let offset = u64::from_le_bytes(row[4..12].try_into().unwrap()) - 6;
                    row[4..12].copy_from_slice(&offset.to_le_bytes());
                }
            }
            "duplicate" => {
                let row = postings[..6].to_vec();
                postings.splice(6..6, row);
                lookup[12..16].copy_from_slice(&2u32.to_le_bytes());
                for row in lookup.chunks_exact_mut(16).skip(1) {
                    let offset = u64::from_le_bytes(row[4..12].try_into().unwrap()) + 6;
                    row[4..12].copy_from_slice(&offset.to_le_bytes());
                }
            }
            _ => postings[..4].copy_from_slice(&9u32.to_le_bytes()),
        }
        fs::write(index.join("lookup.bin"), lookup).unwrap();
        fs::write(index.join("index.bin"), postings).unwrap();
        refresh_trigram_count(&index);
        assert!(
            IndexSession::verify_content(&index, &content_limits()).is_err(),
            "{change}"
        );
    }
}

#[test]
fn content_verification_preadmits_all_size_count_and_work_bounds() {
    for field in [
        "control", "index", "file", "total", "files", "trigrams", "postings", "work",
    ] {
        let (_temp, root, index) = fixture(&[("a.txt", "ordinary copper wire\n")]);
        // Missing text must not precede any of these admission failures.
        fs::remove_file(root.join("a.txt")).unwrap();
        let mut limits = content_limits();
        match field {
            "control" => limits.max_control_bytes = 1,
            "index" => limits.max_index_bytes = 1,
            "file" => limits.max_file_bytes = 1,
            "total" => limits.max_total_text_bytes = 1,
            "files" => limits.max_files = 0,
            "trigrams" => limits.max_trigrams = 0,
            "postings" => limits.max_postings = 0,
            _ => limits.max_work_bytes = 1,
        }
        assert!(
            IndexSession::verify_content(&index, &limits)
                .unwrap_err()
                .to_string()
                .contains("limit"),
            "{field}"
        );
    }
}

#[test]
fn reused_session_matches_single_searches_and_preserves_scope_and_admission() {
    let (_temp, _root, index) = fixture(&[
        ("a.txt", "alpha.beta\nKEY\n中国文档\n"),
        ("b.txt", "alphaXbeta\nKEY\n"),
    ]);
    let session = IndexSession::open(&index).unwrap();
    for (pattern, insensitive, literal, limit) in [
        ("alpha.beta", false, true, 10),
        ("^alpha.beta$", false, false, 1),
        ("key", true, true, 10),
        ("中国", false, true, 10),
        ("missing-token", false, true, 10),
    ] {
        let reused = session
            .search(pattern, insensitive, literal, limit)
            .unwrap();
        let single = search(&index, pattern, insensitive, literal, limit).unwrap();
        assert_eq!(reused.hits, single.hits);
        assert_eq!(reused.candidate_files, single.candidate_files);
        assert_eq!(reused.verified_files, single.verified_files);
        assert_eq!(reused.truncated, single.truncated);
    }
    let mut visited = Vec::new();
    let result = session
        .search_with_document_and_predicate(
            "alpha",
            false,
            true,
            1,
            Some(Path::new("b.txt")),
            |relative| {
                visited.push(relative.to_owned());
                Ok(true)
            },
        )
        .unwrap();
    assert_eq!(visited, [PathBuf::from("b.txt")]);
    assert_eq!(result.hits[0].path, Path::new("b.txt"));
    assert!(
        session
            .search_with_document("alpha", false, true, 1, Some(Path::new("../a.txt")))
            .is_err()
    );
}

#[test]
fn reused_session_rechecks_changed_and_deleted_candidate_text() {
    let (_temp, root, index) = fixture(&[("a.txt", "matching token\n")]);
    let session = IndexSession::open(&index).unwrap();
    assert_eq!(
        session
            .search("matching", false, true, 10)
            .unwrap()
            .hits
            .len(),
        1
    );
    fs::write(root.join("a.txt"), "matching edit!\n").unwrap();
    assert!(
        session
            .search("matching", false, true, 10)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    fs::remove_file(root.join("a.txt")).unwrap();
    assert!(
        session
            .search("matching", false, true, 10)
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
}

#[test]
fn reused_session_rejects_changed_and_missing_index_artifacts() {
    for name in [
        "gptgrep-index.json",
        "meta.json",
        "files-extra.bin",
        "filestamps.json",
        "files.bin",
    ] {
        let (_temp, _root, index) = fixture(&[("a.txt", "matching token\n")]);
        let session = IndexSession::open(&index).unwrap();
        session.search("matching", false, true, 10).unwrap();
        let path = index.join(name);
        let bytes = fs::read(&path).unwrap();
        let mut corrupted = bytes.clone();
        corrupted[0] ^= 1;
        fs::write(&path, corrupted).unwrap();
        assert!(
            session.search("matching", false, true, 10).is_err(),
            "{name}"
        );
        fs::write(&path, bytes).unwrap();
        fs::remove_file(path).unwrap();
        assert!(
            session.search("matching", false, true, 10).is_err(),
            "{name}"
        );
    }
}

#[test]
fn old_engine_session_requires_rebuild_before_reading_new_artifacts() {
    let (_temp, _root, index) = fixture(&[("a.txt", "matching token\n")]);
    let path = index.join("gptgrep-index.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["engine_revision"] = "legacy-engine".into();
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fs::remove_file(index.join("filestamps.json")).unwrap();
    let error = match IndexSession::open(&index) {
        Ok(_) => panic!("old engine must not open"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("rebuild_required"));
}

#[cfg(unix)]
#[test]
fn reused_session_rejects_root_and_artifact_symlinks_and_replaced_postings() {
    use std::os::unix::fs::symlink;
    let (temp, root, index) = fixture(&[("a.txt", "matching token\n")]);
    let session = IndexSession::open(&index).unwrap();
    session.search("matching", false, true, 10).unwrap();
    let saved = temp.path().join("saved-text");
    fs::rename(&root, &saved).unwrap();
    symlink(&saved, &root).unwrap();
    assert!(session.search("matching", false, true, 10).is_err());
    fs::remove_file(&root).unwrap();
    fs::rename(saved, &root).unwrap();
    for name in ["gptgrep-index.json", "index.bin", "lookup.bin"] {
        let path = index.join(name);
        let saved = temp.path().join(format!("saved-{name}"));
        fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(
            session.search("matching", false, true, 10).is_err(),
            "{name}"
        );
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"damaged").unwrap();
        assert!(
            session.search("matching", false, true, 10).is_err(),
            "{name}"
        );
        fs::remove_file(&path).unwrap();
        fs::rename(saved, path).unwrap();
    }
}

#[test]
fn literal_is_verified_and_regex_matches_all_fallback() {
    let (_temp, _root, index) = fixture(&[
        ("a.txt", "alpha.beta\nalphaXbeta\nx\n"),
        ("b.txt", "unrelated\n"),
    ]);
    let literal = search(&index, "alpha.beta", false, true, 10).unwrap();
    assert_eq!(literal.hits.len(), 1);
    assert_eq!(literal.hits[0].line, "alpha.beta");
    let regex = search(&index, "^.$", false, false, 10).unwrap();
    assert_eq!(regex.hits.len(), 1);
    assert_eq!(regex.hits[0].line_number, 3);
    assert_eq!(regex.candidate_files, 2);
    assert_eq!(regex.verified_files, 2);
    let short = search(&index, "x", false, true, 10).unwrap();
    assert_eq!(short.candidate_files, 2);
    assert_eq!(short.hits[0].line, "x");
}

#[test]
fn full_regex_alternation_and_unicode() {
    let (_temp, _root, index) = fixture(&[
        ("a.txt", "中国文档\nalpha 123 tail\n"),
        ("b.txt", "beta 456 tail\nno\n"),
    ]);
    let unicode = search(&index, "中国", false, true, 10).unwrap();
    assert_eq!(unicode.hits[0].line, "中国文档");
    let matches = search(&index, r"^(alpha|beta) \d+ tail$", false, false, 10).unwrap();
    assert_eq!(matches.hits.len(), 2);
    assert_eq!(matches.hits[0].path, Path::new("a.txt"));
    assert_eq!(matches.hits[1].path, Path::new("b.txt"));
}

#[test]
fn unicode_case_folding_does_not_lose_candidates() {
    let (_temp, _root, index) =
        fixture(&[("a.txt", "ÄBC\nKEY\nſea\nMiXeD\n"), ("b.txt", "other\n")]);
    for (pattern, expected) in [
        ("äbc", "ÄBC"),
        ("key", "KEY"),
        ("sea", "ſea"),
        ("mixed", "MiXeD"),
    ] {
        let result = search(&index, pattern, true, true, 10).unwrap();
        assert_eq!(result.hits[0].line, expected, "{pattern}");
    }
    assert!(
        search(&index, "mixed", false, true, 10)
            .unwrap()
            .hits
            .is_empty()
    );
    assert_eq!(
        search(&index, "(?i:key)", false, false, 10).unwrap().hits[0].line,
        "KEY"
    );
}

#[test]
fn limits_report_observed_extra_match_and_no_match_is_success() {
    let (_temp, _root, index) = fixture(&[("a.txt", "hit\nhit\n"), ("b.txt", "miss\n")]);
    let limited = search(&index, "hit", false, true, 1).unwrap();
    assert_eq!(limited.hits.len(), 1);
    assert!(limited.truncated);
    assert_eq!(limited.verified_files, 1);
    let exact = search(&index, "hit", false, true, 2).unwrap();
    assert!(!exact.truncated);
    let absent = search(&index, "unfindable-token", false, true, 5).unwrap();
    assert!(absent.hits.is_empty());
    assert_eq!(absent.candidate_files, 0);
    assert!(!absent.truncated);
    assert!(search(&index, "hit", false, true, 0).is_err());
    assert!(search(&index, "[", false, false, 5).is_err());
}

#[test]
fn missing_or_changed_candidates_fail() {
    let (_temp, root, index) = fixture(&[("a.txt", "matching token\n")]);
    fs::write(root.join("a.txt"), "matching edit!\n").unwrap();
    assert!(
        search(&index, "matching", false, true, 10)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    fs::remove_file(root.join("a.txt")).unwrap();
    assert!(
        search(&index, "matching", false, true, 10)
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
}

#[test]
fn missing_manifest_and_incomplete_core_fail() {
    let (_temp, _root, index) = fixture(&[("a.txt", "matching token\n")]);
    let manifest = index.join("gptgrep-index.json");
    let saved = fs::read(&manifest).unwrap();
    fs::remove_file(&manifest).unwrap();
    assert!(search(&index, "matching", false, true, 10).is_err());
    fs::write(&manifest, saved).unwrap();
    let metadata_path = index.join("meta.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    metadata["complete"] = false.into();
    fs::write(metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(
        search(&index, "matching", false, true, 10)
            .unwrap_err()
            .to_string()
            .contains("coverage")
    );
}

#[test]
fn common_generation_can_move_without_staging_absolute_paths() {
    let (temp, _root, index) = fixture(&[("nested/a.txt", "portable token\n")]);
    let target = TempDir::new().unwrap();
    let published = target.path().join("published");
    fs::rename(temp.path(), &published).unwrap();
    assert!(!index.exists());
    let result = search(&published.join("trigram"), "portable", false, true, 10).unwrap();
    assert_eq!(result.hits[0].path, Path::new("nested/a.txt"));
}

#[test]
fn empty_text_corpus_is_supported() {
    let (_temp, _root, index) = fixture(&[("empty.txt", "")]);
    let result = search(&index, ".*", false, false, 10).unwrap();
    assert!(result.hits.is_empty());
    assert_eq!(result.verified_files, 1);
}

#[test]
fn utf8_bom_is_preserved_for_anchors_and_literal_queries() {
    let (_temp, _root, index) = fixture(&[
        ("bom.txt", "\u{feff}# Alpha\ntext\n"),
        ("plain.txt", "# Alpha\ntext\n"),
    ]);
    let anchored = search(&index, "^# Alpha$", false, false, 10).unwrap();
    assert_eq!(anchored.hits.len(), 1);
    assert_eq!(anchored.hits[0].path, Path::new("plain.txt"));
    for (pattern, literal) in [("\u{feff}# Alpha", true), (r"^\x{FEFF}# Alpha$", false)] {
        let result = search(&index, pattern, false, literal, 10).unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, Path::new("bom.txt"));
        assert_eq!(result.hits[0].line, "\u{feff}# Alpha");
        let direct = if literal {
            regex::escape(pattern)
        } else {
            pattern.into()
        };
        assert!(
            regex::Regex::new(&direct)
                .unwrap()
                .is_match(&result.hits[0].line)
        );
    }
}

#[test]
fn document_filter_precedes_matching_verification_and_result_limits() {
    let unrelated = "needle unrelated\n".repeat(10050);
    let (_temp, root, index) = fixture(&[("a.txt", &unrelated), ("z.txt", "needle target\n")]);
    let unscoped = search(&index, "needle", false, true, 2).unwrap();
    assert!(unscoped.truncated);
    assert!(
        unscoped
            .hits
            .iter()
            .all(|hit| hit.path == Path::new("a.txt"))
    );
    let scoped =
        search_with_document(&index, "needle", false, true, 1, Some(Path::new("z.txt"))).unwrap();
    assert_eq!(scoped.hits.len(), 1);
    assert_eq!(scoped.hits[0].path, Path::new("z.txt"));
    assert_eq!((scoped.candidate_files, scoped.verified_files), (1, 1));
    assert!(!scoped.truncated);
    // An unrelated corrupted candidate is not opened during a scoped search.
    fs::write(root.join("a.txt"), "changed unrelated snapshot").unwrap();
    assert!(
        search_with_document(&index, "needle", false, true, 1, Some(Path::new("z.txt"))).is_ok()
    );
}

#[test]
fn scoped_unicode_casefold_bom_and_match_all_are_verified() {
    let (_temp, _root, index) = fixture(&[
        ("a.txt", "中国\nKEY\n"),
        ("目录/研究.txt", "中国文档\nKEY\n"),
        ("目录/bom.txt", "\u{feff}# Alpha\n"),
    ]);
    for (pattern, insensitive, expected) in [("中国", false, "中国文档"), ("key", true, "KEY")]
    {
        let result = search_with_document(
            &index,
            pattern,
            insensitive,
            true,
            2,
            Some(Path::new("目录/研究.txt")),
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line, expected);
        assert_eq!((result.candidate_files, result.verified_files), (1, 1));
    }
    let bom = search_with_document(
        &index,
        r"^\x{FEFF}# Alpha$",
        false,
        false,
        2,
        Some(Path::new("目录/bom.txt")),
    )
    .unwrap();
    assert_eq!(bom.hits.len(), 1);
    assert_eq!(bom.hits[0].line, "\u{feff}# Alpha");
    assert_eq!(bom.candidate_files, 1);
}

#[test]
fn document_scope_rejects_unknown_and_non_normalized_paths() {
    let (_temp, _root, index) = fixture(&[("nested/a.txt", "needle\n")]);
    for document in [
        "missing.txt",
        "",
        "../a.txt",
        "/a.txt",
        "./nested/a.txt",
        "nested//a.txt",
        "nested/./a.txt",
        "nested/a.txt/",
    ] {
        assert!(
            search_with_document(&index, "needle", false, true, 2, Some(Path::new(document)))
                .is_err(),
            "{document}"
        );
    }
    let absent = search_with_document(
        &index,
        "unfindable-token",
        false,
        true,
        2,
        Some(Path::new("nested/a.txt")),
    )
    .unwrap();
    assert!(absent.hits.is_empty());
    assert_eq!(absent.candidate_files, 0);
}

#[test]
fn candidate_predicate_runs_after_filtering_and_before_verification_and_caps() {
    let stale_matches = "needle old\n".repeat(10050);
    let (_temp, root, index) = fixture(&[
        ("a.txt", &stale_matches),
        ("m.txt", "zzzzzz\n"),
        ("z.txt", "needle fresh\n"),
    ]);
    // The rejected candidate would fail snapshot verification if opened.
    fs::write(root.join("a.txt"), "changed snapshot").unwrap();
    let mut visited = Vec::new();
    let result =
        search_with_document_and_predicate(&index, "needle", false, true, 1, None, |path| {
            visited.push(path.to_owned());
            Ok(path != Path::new("a.txt"))
        })
        .unwrap();
    assert_eq!(visited, [PathBuf::from("a.txt"), PathBuf::from("z.txt")]);
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].path, Path::new("z.txt"));
    assert_eq!((result.candidate_files, result.verified_files), (2, 1));
    assert!(!result.truncated);

    visited.clear();
    search_with_document_and_predicate(
        &index,
        "needle",
        false,
        true,
        1,
        Some(Path::new("z.txt")),
        |path| {
            visited.push(path.to_owned());
            Ok(true)
        },
    )
    .unwrap();
    assert_eq!(visited, [PathBuf::from("z.txt")]);

    let error = search_with_document_and_predicate(&index, "needle", false, true, 1, None, |_| {
        Err(anyhow::anyhow!("candidate admission failed"))
    })
    .unwrap_err();
    assert!(error.to_string().contains("candidate admission failed"));
}

#[test]
fn indexed_regex_equals_direct_scan_for_flags_unicode_and_literals() {
    let files = [
        (
            "a.txt",
            "FOObar\nfooBAR\nFOOBAR\nfoobar\nKEY\nſea\nCAFÉ\ncafé\nＡＢＣ\nαβγ\na.b+[x]\nshort x\n",
        ),
        ("bom.txt", "\u{feff}# Alpha\nFOObar\nſEAbaz\nother\n"),
        ("nested/c.txt", "KEY\nsea\nａｂｃ\nαΒΓ\nfooBAR\naXbxx\n"),
    ];
    let (_temp, _root, index) = fixture(&files);
    for (pattern, insensitive, literal) in [
        ("(?i)foobar", false, false),
        ("(?i:foo)bar", false, false),
        ("foo(?i:bar)", false, false),
        ("(?i:foo)(?-i:bar)", false, false),
        ("(?i:foo)(?-i:BAR)", false, false),
        ("(?i:key|sea)", false, false),
        ("(?i:sea)baz", false, false),
        ("(?i:CAFÉ)", false, false),
        ("(?i:ＡＢＣ)", false, false),
        ("(?i:αβγ)", false, false),
        ("key", true, true),
        ("sea", true, true),
        ("café", true, true),
        ("ＡＢＣ", true, true),
        ("a.b+[x]", false, true),
        ("(?i)foobar", false, true),
        ("^.*$", false, false),
        ("x", false, true),
        ("^# Alpha$", false, false),
        (r"^\x{FEFF}# Alpha$", false, false),
        ("\u{feff}# Alpha", false, true),
    ] {
        let escaped = if literal {
            regex::escape(pattern)
        } else {
            pattern.into()
        };
        let source = if insensitive {
            format!("(?i:{escaped})")
        } else {
            escaped
        };
        let matcher = regex::Regex::new(&source).unwrap();
        let mut direct = Vec::new();
        for (path, text) in &files {
            for (line, text) in text.lines().enumerate() {
                if matcher.is_match(text) {
                    direct.push(Match {
                        path: PathBuf::from(path),
                        line_number: line + 1,
                        line: text.into(),
                    });
                }
            }
        }
        let indexed = search(&index, pattern, insensitive, literal, 100).unwrap();
        assert_eq!(indexed.hits, direct, "{source}");
        assert!(!indexed.truncated, "{source}");
    }
}

#[test]
fn old_engine_manifest_requires_rebuild_and_keeps_existing_artifacts() {
    let (_temp, _root, index) = fixture(&[("a.txt", "needle\n")]);
    let path = index.join("gptgrep-index.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["engine_revision"] = "239711cfb6e69e8780cabf912a8987162a223ff1".into();
    let old_manifest = serde_json::to_vec(&manifest).unwrap();
    fs::write(&path, &old_manifest).unwrap();
    let file_table = fs::read(index.join("files.bin")).unwrap();
    let error = search(&index, "needle", false, true, 10).unwrap_err();
    assert!(error.to_string().contains("rebuild_required"));
    assert_eq!(fs::read(&path).unwrap(), old_manifest);
    assert_eq!(fs::read(index.join("files.bin")).unwrap(), file_table);
}

#[test]
fn unsafe_paths_in_the_index_file_table_fail_before_matching() {
    let (_temp, _root, index) = fixture(&[("safe.txt", "needle\n")]);
    let path = index.join("files.bin");
    let saved = fs::read(&path).unwrap();
    let position = saved
        .windows(b"safe.txt".len())
        .position(|bytes| bytes == b"safe.txt")
        .unwrap();
    for unsafe_path in [b"../x.txt", b"/bad.txt"] {
        let mut corrupt = saved.clone();
        corrupt[position..position + unsafe_path.len()].copy_from_slice(unsafe_path);
        fs::write(&path, corrupt).unwrap();
        let error = search(&index, "needle", false, true, 10).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("normalized root-relative path"),
            "{message}"
        );
    }
    fs::write(path, saved).unwrap();
    assert_eq!(
        search(&index, "needle", false, true, 10)
            .unwrap()
            .hits
            .len(),
        1
    );
}

#[test]
fn truncated_postings_fail_instead_of_returning_incomplete_results() {
    let (_temp, _root, index) = fixture(&[("a.txt", "needle\n")]);
    let path = index.join("index.bin");
    let saved = fs::read(&path).unwrap();
    fs::write(&path, &saved[..saved.len() - 1]).unwrap();
    assert!(search(&index, "needle", false, true, 10).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_snapshot_files_directories_and_root_are_rejected() {
    use std::os::unix::fs::symlink;

    let (temp, root, index) = fixture(&[("nested/a.txt", "needle\n")]);
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("a.txt"), "needle\n").unwrap();
    fs::remove_file(root.join("nested/a.txt")).unwrap();
    symlink(outside.join("a.txt"), root.join("nested/a.txt")).unwrap();
    let error = search(&index, "needle", false, true, 10).unwrap_err();
    assert!(error.to_string().contains("symlink"));

    fs::remove_file(root.join("nested/a.txt")).unwrap();
    fs::remove_dir(root.join("nested")).unwrap();
    symlink(&outside, root.join("nested")).unwrap();
    let error = search(&index, "needle", false, true, 10).unwrap_err();
    assert!(error.to_string().contains("symlink"));

    fs::remove_file(root.join("nested")).unwrap();
    fs::remove_dir(&root).unwrap();
    symlink(&outside, &root).unwrap();
    let error = search(&index, "needle", false, true, 10).unwrap_err();
    assert!(error.to_string().contains("invalid text root"));
}
