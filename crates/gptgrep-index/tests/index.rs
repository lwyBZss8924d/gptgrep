use std::fs;
use std::path::{Path, PathBuf};

use gptgrep_index::{
    Match, build, search, search_with_document, search_with_document_and_predicate,
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
