use super::*;
use crate::RichGenerationWriter;
use anyhow::Result;
use gptgrep_pageindex::{PageInput, from_pages};
use gptgrep_parse::RichParsedDocument;
use gptgrep_parse::{
    AffineTransform, ArtifactKind, ArtifactRef, CoverageState, FeatureState, NativePage,
    PageGeometry, PreparedSummary, RICH_PARSER, ResourceBudget,
};
use serde_json::json;
use std::{collections::BTreeMap, fs};
fn byte_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn candidate(root: &Path, texts: &[&str]) -> Result<(tempfile::TempDir, RichParsedDocument)> {
    let staging = tempfile::tempdir()?;
    let staging_path = staging.path().canonicalize()?;
    let source = b"synthetic source bytes for artifact custody tests";
    fs::write(root.join("source.pdf"), source)?;
    fs::write(staging_path.join("prepared.pdf"), source)?;
    let sha = byte_digest(source);
    let profile = byte_digest(b"synthetic parser recipe");
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut create = |kind: ArtifactKind,
                      page: Option<u32>,
                      bytes: &[u8],
                      name: &str|
     -> Result<ArtifactRef> {
        let artifact = ArtifactRef {
            id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &byte_digest(bytes))?,
            kind,
            relative_path: name.into(),
            sha256: byte_digest(bytes),
            profile_sha256: profile.clone(),
            bytes: bytes.len() as u64,
            mime: if name.ends_with(".txt") {
                "text/plain; charset=utf-8"
            } else {
                "application/json"
            }
            .into(),
            page,
            region_id: None,
        };
        fs::write(staging_path.join(name), bytes)?;
        artifacts.push(artifact.clone());
        Ok(artifact)
    };
    for (index, text) in texts.iter().enumerate() {
        let number = index as u32 + 1;
        let native_text = create(
            ArtifactKind::NativeText,
            Some(number),
            text.as_bytes(),
            &format!("p{number}-native.txt"),
        )?;
        let geometry = PageGeometry {
            width_points: 100.0,
            height_points: 200.0,
            rotation_quarter_turns: 0,
            user_unit: 1.0,
            source_to_page: AffineTransform::IDENTITY,
            page_to_source: AffineTransform::IDENTITY,
        };
        let complexity = serde_json::from_value(
            json!({"page_number":number,"text_length":text.len(),"text_coverage":0.5,
            "has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,
            "full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":20000.0,
            "needs_ocr":false,"reasons":[],"layout":null}),
        )?;
        let capabilities =
            BTreeMap::from([("annotation_count".into(), FeatureState::Present("0".into()))]);
        let metadata_artifact = create(
            ArtifactKind::NativeMetadata,
            Some(number),
            &serde_json::to_vec(
                &json!({"page":{"page_number":number,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
                "capabilities":capabilities,"complexity":complexity,"geometry":geometry,"image_rects":[]}),
            )?,
            &format!("p{number}-native.json"),
        )?;
        pages.push(NativePage {
            number,
            label: FeatureState::Absent,
            width_points: 100.0,
            height_points: 200.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![],
            complexity,
            coverage: CoverageState::Native,
            capabilities,
        });
    }
    let inputs = texts
        .iter()
        .enumerate()
        .map(|(i, text)| PageInput {
            number: i as u32 + 1,
            text: (*text).into(),
        })
        .collect::<Vec<_>>();
    let document = from_pages("synthetic", &inputs, &[], &[], RICH_PARSER)?;
    create(
        ArtifactKind::MergedText,
        None,
        document.text.as_bytes(),
        "merged.txt",
    )?;
    create(ArtifactKind::Layout, None, b"[]", "layout.json")?;
    let unmapped_merged_lines = document.text.lines().count();
    Ok((
        staging,
        RichParsedDocument {
            summary: PreparedSummary {
                source_sha256: sha.clone(),
                prepared_sha256: sha,
                source_bytes: source.len() as u64,
                prepared_bytes: source.len() as u64,
                format: "pdf".into(),
                parser_identity: RICH_PARSER.into(),
                preparation_profile_sha256: byte_digest(&serde_json::to_vec(&(
                    "gptgrep.prepare.identity-pdf.v1",
                    gptgrep_parse::LITEPARSE_REVISION,
                ))?),
                conversion: FeatureState::Present(gptgrep_parse::ConversionIdentity {
                    converter: "identity-pdf".into(),
                    version: gptgrep_parse::LITEPARSE_REVISION.into(),
                    invocations: 0,
                }),
                image_geometry: FeatureState::Absent,
            },
            document,
            pages,
            artifacts,
            native_profile_sha256: profile,
            segments: vec![],
            ocr_tasks: vec![],
            ocr_results: vec![],
            coverage: texts
                .iter()
                .enumerate()
                .map(|(i, _)| (i as u32 + 1, CoverageState::Native))
                .collect(),
            unmapped_merged_lines,
        },
    ))
}

pub(crate) fn publish(root: &Path, texts: &[&str]) -> Result<GenerationPin> {
    let (staging, parsed) = candidate(root, texts)?;
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()
}

// A real Writer/Reader generation with synthetic source-bound PNG artifacts.
// This exercises local custody; it qualifies no native parser or model delivery.
fn visual_png(value: u8) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = u32::MAX;
        for byte in kind.iter().chain(data) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = vec![];
    header.extend_from_slice(&1u32.to_be_bytes());
    header.extend_from_slice(&1u32.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    let raw = [0, value, 60, 78];
    let mut zlib = vec![0x78, 0x01, 1, 4, 0, 251, 255];
    zlib.extend_from_slice(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for byte in raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

pub(crate) fn publish_visual(root: &Path, pages: usize) -> Result<(GenerationPin, Vec<Vec<u8>>)> {
    let texts = vec!["synthetic visual source text\n"; pages];
    let (staging, mut parsed) = candidate(root, &texts)?;
    let mut images = Vec::new();
    for page in &mut parsed.pages {
        let bytes = visual_png(page.number as u8);
        let name = format!("p{}-rendition.png", page.number);
        let artifact = ArtifactRef {
            id: gptgrep_parse::artifact_id(
                &parsed.summary.source_sha256,
                &parsed.native_profile_sha256,
                &ArtifactKind::Rendition,
                Some(page.number),
                None,
                &byte_digest(&bytes),
            )?,
            kind: ArtifactKind::Rendition,
            relative_path: name.into(),
            sha256: byte_digest(&bytes),
            profile_sha256: parsed.native_profile_sha256.clone(),
            bytes: bytes.len() as u64,
            mime: "image/png".into(),
            page: Some(page.number),
            region_id: None,
        };
        let path = staging.path().join(&artifact.relative_path);
        fs::write(&path, &bytes)?;
        page.renditions.push(gptgrep_parse::OcrImage {
            artifact: artifact.clone(),
            path,
            width: 1,
            height: 1,
            page_to_raster: AffineTransform {
                a: 0.01,
                d: 0.005,
                ..AffineTransform::IDENTITY
            },
        });
        parsed.artifacts.push(artifact);
        images.push(bytes);
    }
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    Ok((writer.publish()?, images))
}

fn decode_visual_base64(value: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = Vec::new();
    for chunk in value.as_bytes().chunks_exact(4) {
        let digit = |byte| alphabet.iter().position(|v| *v == byte).unwrap() as u32;
        let word = (digit(chunk[0]) << 18)
            | (digit(chunk[1]) << 12)
            | if chunk[2] == b'=' {
                0
            } else {
                digit(chunk[2]) << 6
            }
            | if chunk[3] == b'=' { 0 } else { digit(chunk[3]) };
        bytes.push((word >> 16) as u8);
        if chunk[2] != b'=' {
            bytes.push((word >> 8) as u8);
        }
        if chunk[3] != b'=' {
            bytes.push(word as u8);
        }
    }
    bytes
}

#[test]
fn visual_continuation_uses_native_fixed_pin_query_scope_and_options() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let (pin, _) = publish_visual(&root, 2)?;
    let reader = RichGenerationReader::open(&root)?;
    let first = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"visual","op":"inspect_visual","query":"generic visual question","max_candidates":1}
        ])),
    )?;
    let result = first.items[0].result.as_ref().unwrap();
    assert_eq!(result["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(result["actual_image_delivery"], false);
    assert_eq!(result["citation_authority"], false);
    let token = result["next_cursor"].as_str().unwrap();
    assert!(first.items[0].coverage.cursor.is_none());
    let mut next = request(json!([{"id":"visual","op":"inspect_visual",
        "query":"generic visual question","max_candidates":1,"cursor":token}]));
    next.generation = RichBatchGeneration::Exact(pin.clone());
    let second = dispatch_rich_batch_with_reader(&reader, next.clone())?;
    assert_eq!(second.items[0].result.as_ref().unwrap()["offset"], 1);
    assert!(second.items[0].result.as_ref().unwrap()["next_cursor"].is_null());
    for changed in [
        json!({"id":"visual","op":"inspect_visual","query":"other","max_candidates":1,"cursor":token}),
        json!({"id":"visual","op":"inspect_visual","query":"generic visual question","max_candidates":2,"cursor":token}),
        json!({"id":"visual","op":"inspect_visual","scope":{"first_page":2},"query":"generic visual question","max_candidates":1,"cursor":token}),
        json!({"id":"text","op":"inspect_pages","document":"source.pdf","cursor":token}),
    ] {
        let response = dispatch_rich_batch_with_reader(&reader, request(json!([changed])))?;
        assert_eq!(
            response.items[0].error.as_ref().unwrap().code,
            RichBatchErrorCode::InvalidCursor
        );
    }
    let (new_pin, _) = publish_visual(&root, 3)?;
    assert_ne!(pin, new_pin);
    assert_eq!(dispatch_rich_batch(&root, next.clone())?.generation, pin);
    next.generation = RichBatchGeneration::Current(CurrentRichGeneration::Current);
    let response = dispatch_rich_batch(&root, next)?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidCursor
    );
    Ok(())
}

#[test]
fn visual_read_is_exact_complete_local_observation_with_shared_metrics() -> Result<()> {
    for (raw, encoded) in [
        (b"".as_slice(), ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(visual_base64(raw, encoded.len()), encoded);
    }
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let (_, images) = publish_visual(&root, 1)?;
    let reader = RichGenerationReader::open(&root)?;
    let id = &reader.manifest()?.documents[0].pages[0].renditions[0]
        .artifact
        .id;
    let response = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"read","op":"read_visual","rendition_id":id},
            {"id":"catalog","op":"inspect_documents"}
        ])),
    )?;
    let result = response.items[0].result.as_ref().unwrap();
    assert_eq!(
        decode_visual_base64(result["image_base64"].as_str().unwrap()),
        images[0]
    );
    assert_eq!(result["candidate"]["encoded_bytes"], images[0].len());
    assert_eq!(result["actual_image_delivery"], false);
    assert_eq!(result["citation_authority"], false);
    assert!(response.items[0].coverage.exhausted);
    assert!(!response.items[0].coverage.budget_truncated);
    assert!(response.items[0].coverage.cursor.is_none());
    let item_reads: u64 = response
        .items
        .iter()
        .map(|i| i.read_metrics.as_ref().unwrap().observed_rust_read_bytes)
        .sum();
    assert!(
        response
            .metrics
            .read_metrics
            .as_ref()
            .unwrap()
            .observed_rust_read_bytes
            >= item_reads
    );
    assert_eq!(response.metrics.model_submissions, Some(0));
    assert!(response.metrics.physical_bytes.is_none());
    Ok(())
}

#[test]
fn visual_missing_and_stale_sources_are_local_but_artifact_corruption_is_global() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    publish_visual(&root, 1)?;
    let reader = RichGenerationReader::open(&root)?;
    let manifest = reader.manifest()?;
    let artifact = &manifest.documents[0].pages[0].renditions[0].artifact;
    let source = fs::read(root.join("source.pdf"))?;
    for (id, stale) in [("missing", false), (artifact.id.as_str(), true)] {
        if stale {
            fs::write(root.join("source.pdf"), b"changed source")?;
        }
        let response = dispatch_rich_batch_with_reader(
            &reader,
            request(json!([
                {"id":"read","op":"read_visual","rendition_id":id},
                {"id":"later","op":"inspect_documents"}
            ])),
        )?;
        assert_eq!(
            response.items[0].error.as_ref().unwrap().code,
            if stale {
                RichBatchErrorCode::SourceStale
            } else {
                RichBatchErrorCode::ResourceNotFound
            }
        );
        assert_eq!(response.items[1].status, RichBatchStatus::Ok);
    }
    fs::write(root.join("source.pdf"), &source)?;
    let artifact_path = root
        .join(".gptgrep/generations")
        .join(&reader.pin().generation)
        .join(&artifact.relative_path);
    fs::write(&artifact_path, b"corrupted image")?;
    let mut small = request(json!([{"id":"read","op":"read_visual","rendition_id":artifact.id}]));
    small.budget.max_output_bytes = ENVELOPE_RESERVE + MIN_ITEM_OUTPUT;
    let response = dispatch_rich_batch_with_reader(&reader, small)?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert!(response.items[0].result.is_none());
    let failure = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"first","op":"inspect_documents"},
            {"id":"read","op":"read_visual","rendition_id":artifact.id},
            {"id":"later","op":"inspect_documents"}
        ])),
    )
    .unwrap_err();
    assert_eq!(failure.error.code, RichBatchErrorCode::SharedIntegrity);
    assert_eq!(failure.items[0].status, RichBatchStatus::Ok);
    assert_eq!(failure.items[2].status, RichBatchStatus::NotExecuted);
    Ok(())
}

#[test]
fn visual_reservations_do_not_borrow_other_items_and_no_image_cursor_is_admitted() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    publish_visual(&root, 1)?;
    let reader = RichGenerationReader::open(&root)?;
    let id = &reader.manifest()?.documents[0].pages[0].renditions[0]
        .artifact
        .id;
    let mut req = request(json!([
        {"id":"visual","op":"read_visual","rendition_id":id},
        {"id":"catalog","op":"inspect_documents"}
    ]));
    req.budget.max_output_bytes = ENVELOPE_RESERVE + 2 * MIN_ITEM_OUTPUT;
    let response = dispatch_rich_batch_with_reader(&reader, req)?;
    assert_eq!(response.items[0].reserved_output_bytes, MIN_ITEM_OUTPUT);
    assert_eq!(response.items[1].reserved_output_bytes, MIN_ITEM_OUTPUT);
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert_eq!(response.items[1].status, RichBatchStatus::Ok);
    assert!(
        parse_rich_batch_request(&serde_json::to_vec(
            &json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,
        "operations":[{"id":"x","op":"read_visual","rendition_id":id,"cursor":"forbidden"}]})
        )?)
        .is_err()
    );
    Ok(())
}

#[test]
fn visual_counts_and_scope_errors_do_not_read_rendition_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    publish_visual(&root, 1)?;
    let reader = RichGenerationReader::open(&root)?;
    let strict_reader = RichGenerationReader::open_with_cache_budget(
        &root,
        &ReaderCacheBudget {
            strict_cache_only: true,
            ..Default::default()
        },
    )?;
    let doc = &reader.manifest()?.documents[0];
    let artifact = &doc.pages[0].renditions[0].artifact;
    fs::write(
        root.join(".gptgrep/generations")
            .join(&reader.pin().generation)
            .join(&artifact.relative_path),
        b"unread corrupted rendition",
    )?;
    let response = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"zero","op":"inspect_visual","max_candidates":0},
            {"id":"count","op":"inspect_visual","max_candidates":257},
            {"id":"duplicate","op":"inspect_visual","scope":{"document_ids":[doc.document.id,doc.document.id]}},
            {"id":"missing","op":"inspect_visual","scope":{"document_ids":["absent-document"]}},
            {"id":"later","op":"inspect_documents"}
        ])),
    )?;
    for item in &response.items[..3] {
        assert_eq!(
            item.error.as_ref().unwrap().code,
            RichBatchErrorCode::InvalidOperation
        );
    }
    assert_eq!(
        response.items[3].error.as_ref().unwrap().code,
        RichBatchErrorCode::ResourceNotFound
    );
    assert_eq!(response.items[4].status, RichBatchStatus::Ok);
    let mut strict_request = request(json!([
        {"id":"visual","op":"read_visual","rendition_id":artifact.id},
        {"id":"later","op":"inspect_documents"}
    ]));
    strict_request.budget.cache.strict_cache_only = true;
    let response = dispatch_rich_batch_with_reader(&strict_reader, strict_request)?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidOperation
    );
    assert_eq!(response.items[1].status, RichBatchStatus::Ok);
    Ok(())
}

#[test]
fn rich_batch_schema_rejects_invalid_envelopes() {
    assert!(parse_rich_batch_request(b"{}").is_err());
}

pub(super) fn request(ops: Value) -> RichBatchRequest {
    parse_rich_batch_request(
        &serde_json::to_vec(&json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":ops}))
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn strict_rich_schema_all_ids_and_declarations_preflight_before_io() -> Result<()> {
    for value in [
        json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":[{"op":"inspect_documents","id":"x","unknown":1}]}),
        json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"budget":{"max_output_bytes":MAX_RICH_BATCH_OUTPUT_BYTES+1},"operations":[{"op":"inspect_documents","id":"x"}]}),
        json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":[{"op":"inspect_documents","id":"x"},{"op":"inspect_documents","id":"x"}]}),
        json!({"schema_version":"gptgrep.batch.request.v1","operations":[{"op":"inspect_documents","id":"x"}]}),
        json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"generation":{"generation":"../bad","manifest_sha256":"a".repeat(64)},"operations":[{"op":"inspect_documents","id":"x"}]}),
        json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"budget":{"cache":{"max_bytes":0}},"operations":[{"op":"inspect_documents","id":"x"}]}),
    ] {
        assert!(parse_rich_batch_request(&serde_json::to_vec(&value)?).is_err());
    }
    assert!(parse_rich_batch_request(&vec![b' '; MAX_RICH_BATCH_INPUT_BYTES + 1]).is_err());
    assert!(parse_rich_batch_request(br#"{"schema_version":"gptgrep.batch.rich.request.v1","operations":[{"id":"one","id":"two","op":"inspect_documents"}]}"#).is_err());
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["needle one\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let req = request(json!([
        {"id":"fields","op":"inspect_pages","document":"source.pdf","fields":["complexity","bogus"]},
        {"id":"regex","op":"search_artifacts","query":"["},
        {"id":"later","op":"inspect_documents","fields":["title"]}
    ]));
    let response = dispatch_rich_batch_with_reader(&reader, req)?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidFields
    );
    assert_eq!(
        response.items[1].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidRegex
    );
    assert_eq!(response.items[2].status, RichBatchStatus::Ok);
    assert_eq!(response.metrics.model_submissions, Some(0));
    assert!(response.metrics.physical_bytes.is_none());
    Ok(())
}

#[test]
fn inspection_cursor_projection_is_complete_and_bound_to_every_option() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let pin = publish(&root, &["first\n", "second\n", "third\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let first = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"pages","op":"inspect_pages","document":"source.pdf","fields":["coverage","native_text"],"page_size":1}]),
        ),
    )?;
    let object = &first.items[0].result.as_ref().unwrap()["objects"][0];
    assert_eq!(object["page"], 1);
    assert_eq!(object["source_status"], "indexed_metadata_only");
    assert!(object.get("complexity").is_none());
    assert_eq!(
        object["native_text"]["id"],
        reader.manifest()?.documents[0].pages[0].native_text.id
    );
    let token = first.items[0].coverage.cursor.as_ref().unwrap();
    let next = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"pages","op":"inspect_pages","document":"source.pdf","fields":["coverage","native_text"],"page_size":1,"cursor":token}]),
        ),
    )?;
    assert_eq!(
        next.items[0].result.as_ref().unwrap()["objects"][0]["page"],
        2
    );
    for op in [
        json!({"id":"changed","op":"inspect_pages","document":"source.pdf","fields":["native_text"],"page_size":1,"cursor":token}),
        json!({"id":"changed","op":"inspect_pages","document":"source.pdf","fields":["coverage","native_text"],"page_size":2,"cursor":token}),
        json!({"id":"changed","op":"inspect_pages","document":"other.pdf","fields":["coverage","native_text"],"page_size":1,"cursor":token}),
        json!({"id":"changed","op":"inspect_documents","page_size":1,"cursor":token}),
    ] {
        let result = dispatch_rich_batch_with_reader(&reader, request(json!([op])))?;
        assert_eq!(
            result.items[0].error.as_ref().unwrap().code,
            RichBatchErrorCode::InvalidCursor
        );
    }
    let newer = publish(&root, &["different\n"])?;
    assert_ne!(newer, pin);
    let old = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"old","op":"inspect_pages","document":"source.pdf","fields":["coverage","native_text"],"page_size":1,"cursor":token}]),
        ),
    )?;
    assert_eq!(old.generation, pin);
    let current = RichGenerationReader::open(&root)?;
    let bad = dispatch_rich_batch_with_reader(
        &current,
        request(
            json!([{"id":"bad","op":"inspect_pages","document":"source.pdf","fields":["coverage","native_text"],"page_size":1,"cursor":token}]),
        ),
    )?;
    assert_eq!(
        bad.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidCursor
    );
    let mut exact = request(json!([{"id":"exact","op":"inspect_documents"}]));
    exact.generation = RichBatchGeneration::Exact(pin.clone());
    assert_eq!(dispatch_rich_batch(&root, exact)?.generation, pin);
    Ok(())
}

#[test]
fn read_artifact_utf8_continuation_replays_exact_source_bound_windows() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let text = "A中β\r\nneedle tail\n";
    publish(&root, &[text])?;
    let reader = RichGenerationReader::open(&root)?;
    let id = reader.manifest()?.documents[0].pages[0]
        .native_text
        .id
        .clone();
    let small_window = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"small","op":"read_artifact","artifact_id":id,"offset":1,"max_bytes":1},
            {"id":"later","op":"inspect_documents","fields":["title"]}
        ])),
    )?;
    assert_eq!(small_window.items[0].status, RichBatchStatus::Error);
    assert_eq!(
        small_window.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidOperation
    );
    assert!(small_window.items[0].coverage.cursor.is_none());
    assert_eq!(small_window.items[1].status, RichBatchStatus::Ok);
    assert_eq!(small_window.coverage.failed, 1);
    assert_eq!(small_window.coverage.succeeded, 1);
    assert_eq!(small_window.coverage.not_executed, 0);
    let mut token = None;
    let mut reconstructed = String::new();
    let mut windows = 0;
    loop {
        let response = dispatch_rich_batch_with_reader(
            &reader,
            request(
                json!([{"id":"read","op":"read_artifact","artifact_id":id,"max_bytes":4,"cursor":token}]),
            ),
        )?;
        let item = &response.items[0];
        assert_eq!(item.status, RichBatchStatus::Ok);
        let window: crate::ArtifactTextWindow =
            serde_json::from_value(item.result.clone().unwrap())?;
        reader.validate_text_evidence(&window.evidence)?;
        assert!(matches!(
            window.evidence,
            crate::EvidenceRef::NativeText { .. }
        ));
        assert!(!window.text.is_empty());
        reconstructed.push_str(&window.text);
        windows += 1;
        token = item.coverage.cursor.clone();
        if token.is_none() {
            break;
        }
        if windows == 1 {
            let changed = dispatch_rich_batch_with_reader(
                &reader,
                request(
                    json!([{"id":"changed","op":"read_artifact","artifact_id":id,"max_bytes":5,"cursor":token}]),
                ),
            )?;
            assert_eq!(
                changed.items[0].error.as_ref().unwrap().code,
                RichBatchErrorCode::InvalidCursor
            );
        }
    }
    assert_eq!(reconstructed, text);
    assert!(windows > 2);
    let invalid = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"bad","op":"read_artifact","artifact_id":id,"offset":2,"max_bytes":4}]),
        ),
    )?;
    assert_eq!(
        invalid.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidOperation
    );
    Ok(())
}

#[test]
fn bounded_search_cursors_bind_query_and_truthfully_report_ceiling() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["needle one\nneedle two\nneedle three\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let first = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"search","op":"search_artifacts","query":"needle","literal":true,"max_hits":2,"page_size":1}]),
        ),
    )?;
    assert_eq!(first.items[0].status, RichBatchStatus::Ok);
    let token = first.items[0].coverage.cursor.as_ref().unwrap();
    let next = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"search","op":"search_artifacts","query":"needle","literal":true,"max_hits":2,"page_size":1,"cursor":token}]),
        ),
    )?;
    assert!(next.items[0].coverage.cursor.is_none());
    assert!(!next.items[0].coverage.exhausted);
    assert!(next.items[0].coverage.search_ceiling_reached);
    for extra in [
        json!({"query":"one"}),
        json!({"literal":false}),
        json!({"max_hits":3}),
        json!({"page_size":2}),
        json!({"case_insensitive":true}),
    ] {
        let mut op = json!({"id":"changed","op":"search_artifacts","query":"needle","literal":true,"max_hits":2,"page_size":1,"cursor":token});
        for (k, v) in extra.as_object().unwrap() {
            op[k] = v.clone();
        }
        let bad = dispatch_rich_batch_with_reader(&reader, request(json!([op])))?;
        assert_eq!(
            bad.items[0].error.as_ref().unwrap().code,
            RichBatchErrorCode::InvalidCursor
        );
    }
    for item in [&first.items[0], &next.items[0]] {
        let hit: crate::ArtifactTextWindow =
            serde_json::from_value(item.result.as_ref().unwrap()["hits"][0].clone())?;
        reader.validate_text_evidence(&hit.evidence)?;
    }
    Ok(())
}

#[test]
fn reservations_preserve_later_items_and_reject_nonprogress_metadata() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (staging, mut parsed) = candidate(&root, &["needle\n"])?;
    parsed.document.warnings = vec!["long serialized warning ".repeat(4000)];
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()?;
    let reader = RichGenerationReader::open(&root)?;
    let mut req = request(json!([
        {"id":"too-big","op":"inspect_documents","fields":["warnings"],"page_size":1},
        {"id":"later","op":"inspect_pages","document":"source.pdf","fields":["page"],"page_size":1}
    ]));
    req.budget.max_output_bytes = 12_288;
    let response = dispatch_rich_batch_with_reader(&reader, req.clone())?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert!(response.items[0].coverage.cursor.is_none());
    assert_eq!(response.items[1].status, RichBatchStatus::Ok);
    assert!(measure(&response, req.budget.max_output_bytes).is_ok());
    for item in &response.items {
        assert!(measure(item, item.reserved_output_bytes).is_ok());
    }
    Ok(())
}

#[test]
fn source_stale_is_individual_while_manifest_corruption_retains_shared_counts() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let id = reader.manifest()?.documents[0].pages[0]
        .native_text
        .id
        .clone();
    fs::write(root.join("source.pdf"), "changed")?;
    let req = request(json!([
        {"id":"stale","op":"read_artifact","artifact_id":id,"max_bytes":4},
        {"id":"metadata","op":"inspect_documents","fields":["title"]}
    ]));
    let before = reader.read_metrics()?;
    let response = dispatch_rich_batch_with_reader(&reader, req)?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::SourceStale
    );
    assert_eq!(response.items[1].status, RichBatchStatus::Ok);
    assert_eq!(
        response.items[1].result.as_ref().unwrap()["source_status"],
        "indexed_metadata_only"
    );
    let expected = meter_delta(&reader.read_metrics()?, &before)?;
    assert_eq!(response.metrics.read_metrics.as_ref().unwrap(), &expected);
    let item_reads: u64 = response
        .items
        .iter()
        .map(|i| i.read_metrics.as_ref().unwrap().observed_rust_read_bytes)
        .sum();
    assert!(item_reads < expected.observed_rust_read_bytes);
    fs::write(
        root.join(".gptgrep/generations")
            .join(&reader.pin().generation)
            .join("manifest.json"),
        "{}",
    )?;
    let failed=dispatch_rich_batch_with_reader(&reader,request(json!([{"id":"first","op":"inspect_documents"},{"id":"later","op":"inspect_documents"}]))) .unwrap_err();
    assert_eq!(failed.error.code, RichBatchErrorCode::SharedIntegrity);
    assert!(
        failed
            .items
            .iter()
            .all(|i| i.status == RichBatchStatus::NotExecuted)
    );
    assert_eq!(
        failed
            .read_metrics
            .as_ref()
            .unwrap()
            .observed_rust_read_bytes,
        2
    );
    Ok(())
}
fn native_pdf_fixture() -> Vec<u8> {
    let body = format!("BT /F1 8 Tf 20 180 Td {} ET", "(Native evidence page with ordinary printable text for parsing and source custody.) Tj 0 -12 Td ".repeat(12));
    let vector = "0 0 1 rg 20 20 80 80 re f BT /F1 8 Tf 20 180 Td (Caption) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{vector}\nendstream", vector.len()),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 200] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()));
    for offset in offsets.iter().skip(1) {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        offsets.len()
    ));
    pdf.into_bytes()
}

#[tokio::test]
async fn native_and_offline_ocr_search_read_keep_distinct_evidence_receipts() -> Result<()> {
    use gptgrep_parse::*;
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    fs::write(root.join("source.pdf"), native_pdf_fixture())?;
    let budget = ResourceBudget::default();
    let prepared = prepare_document(
        &root.join("source.pdf"),
        &root.join("staging"),
        &PreparationOptions,
        &budget,
    )
    .await?;
    let candidate = extract_native(&prepared, &NativeOptions::default(), &budget).await?;
    assert_eq!(
        candidate.plan().tasks.len(),
        1,
        "{:?}",
        candidate
            .pages()
            .iter()
            .map(|p| (
                p.number,
                &p.coverage,
                &p.complexity.reasons,
                p.complexity.text_length
            ))
            .collect::<Vec<_>>()
    );
    let task = &candidate.plan().tasks[0];
    let raw = "Raw\r\n\t图像 label  ";
    // Declared fake receipts test offline DTO custody only, not actual image delivery.
    let result = OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: task.page,
        status: OcrResultStatus::Complete,
        regions: task
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| OcrRegionResult {
                id: r.id.clone(),
                bbox: r.bbox,
                text: raw.into(),
                reading_order: i as u32,
                status: OcrRegionStatus::Complete,
            })
            .collect(),
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: task
                .images
                .iter()
                .enumerate()
                .map(|(i, img)| OcrImageDelivery {
                    artifact_id: img.artifact.id.clone(),
                    artifact_sha256: img.artifact.sha256.clone(),
                    tool_ordinal: i as u32 + 1,
                    mime: img.artifact.mime.clone(),
                    width: img.width,
                    height: img.height,
                    tool_response_sha256: "4".repeat(64),
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    };
    let parsed = finalize_document(candidate, vec![result], OcrMode::Auto, &budget)?;
    let mut writer = RichGenerationWriter::begin(&root, budget)?;
    writer.add_document("source.pdf", prepared.staging_root(), parsed)?;
    writer.publish()?;
    let reader = RichGenerationReader::open(&root)?;
    let manifest = reader.manifest()?;
    let ocr = manifest.documents[0]
        .artifacts
        .iter()
        .find(|a| a.artifact.kind == ArtifactKind::OcrText)
        .unwrap()
        .artifact
        .id
        .clone();
    let req = request(json!([
        {"id":"ocr-search","op":"search_artifacts","query":"图像","literal":true,"max_hits":1,"page_size":1},
        {"id":"native-search","op":"search_artifacts","query":"Native evidence","literal":true,"max_hits":1,"page_size":1},
        {"id":"ocr-read","op":"read_artifact","artifact_id":ocr,"max_bytes":1024}
    ]));
    let response = dispatch_rich_batch_with_reader(&reader, req)?;
    for item in &response.items {
        assert_eq!(item.status, RichBatchStatus::Ok, "{:?}", item.error);
    }
    for (index, expect_ocr) in [(0, true), (1, false)] {
        let hit: crate::ArtifactTextWindow = serde_json::from_value(
            response.items[index].result.as_ref().unwrap()["hits"][0].clone(),
        )?;
        assert_eq!(
            matches!(hit.evidence, crate::EvidenceRef::OcrText { .. }),
            expect_ocr
        );
        reader.validate_text_evidence(&hit.evidence)?;
    }
    let retained: crate::ArtifactTextWindow =
        serde_json::from_value(response.items[2].result.clone().unwrap())?;
    assert_eq!(retained.text, format!("{raw}\n"));
    assert!(matches!(
        retained.evidence,
        crate::EvidenceRef::OcrText { .. }
    ));
    reader.validate_text_evidence(&retained.evidence)?;
    assert_eq!(response.metrics.model_submissions, Some(0));
    Ok(())
}

#[test]
fn supplied_reader_cannot_exceed_request_cache_policy_or_turn_eof_into_citation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["ordinary text\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = reader.manifest()?.documents[0].pages[0].native_text.clone();
    let mut req = request(json!([{"id":"tight","op":"read_artifact","artifact_id":artifact.id}]));
    req.budget.cache.max_bytes = 1;
    let before = reader.read_metrics()?;
    let failed = dispatch_rich_batch_with_reader(&reader, req).unwrap_err();
    assert_eq!(failed.error.code, RichBatchErrorCode::InvalidRequest);
    assert_eq!(reader.read_metrics()?, before);
    let eof = dispatch_rich_batch_with_reader(
        &reader,
        request(
            json!([{"id":"eof","op":"read_artifact","artifact_id":artifact.id,"offset":artifact.bytes,"max_bytes":4}]),
        ),
    )?;
    let window: crate::ArtifactTextWindow =
        serde_json::from_value(eof.items[0].result.clone().unwrap())?;
    assert!(window.text.is_empty());
    assert!(eof.items[0].coverage.exhausted);
    assert!(eof.items[0].coverage.cursor.is_none());
    assert!(reader.validate_text_evidence(&window.evidence).is_err());
    Ok(())
}

#[test]
fn stale_search_preserves_other_documents_and_never_claims_exhaustion() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (staging, parsed) = candidate(&root, &["needle first\n"])?;
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    let (staging, parsed) = candidate(&root, &["needle other\n"])?;
    fs::copy(root.join("source.pdf"), root.join("second.pdf"))?;
    writer.add_document("second.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()?;
    let reader = RichGenerationReader::open(&root)?;
    let second = reader
        .manifest()?
        .documents
        .iter()
        .find(|d| d.document.path == "second.pdf")
        .unwrap()
        .pages[0]
        .native_text
        .id
        .clone();
    fs::write(root.join("source.pdf"), "changed source")?;
    let response = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"partial","op":"search_artifacts","query":"needle","literal":true,"max_hits":2,"page_size":2},
            {"id":"other","op":"read_artifact","artifact_id":second,"max_bytes":1024}
        ])),
    )?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::SourceStale
    );
    assert!(!response.items[0].coverage.exhausted);
    assert_eq!(
        response.items[0].result.as_ref().unwrap()["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(response.items[1].status, RichBatchStatus::Ok);
    assert_eq!(
        response.items[1].result.as_ref().unwrap()["text"],
        "needle other\n"
    );
    Ok(())
}

#[test]
fn escaped_scope_is_admitted_before_cursor_output_allocation() {
    let pin = GenerationPin {
        generation: "retained".into(),
        manifest_sha256: "a".repeat(64),
    };
    let op = RichBatchOperation::InspectPages {
        id: "scope".into(),
        document: "\"".repeat(4096),
        fields: vec![],
        page_size: 1,
        cursor: None,
    };
    assert_eq!(
        preflight(&pin, &op).unwrap_err().code,
        RichBatchErrorCode::InvalidOperation
    );
}

#[test]
fn resource_and_offset_preflight_rejects_before_text_loads() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = reader.manifest()?.documents[0].pages[0].native_text.clone();
    let before = reader.read_metrics()?;
    let response = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"offset","op":"read_artifact","artifact_id":artifact.id,"offset":usize::MAX},
            {"id":"missing","op":"inspect_pages","document":"missing.pdf"},
            {"id":"later","op":"inspect_documents","fields":["title"]}
        ])),
    )?;
    assert_eq!(
        response.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::InvalidOperation
    );
    assert_eq!(
        response.items[1].error.as_ref().unwrap().code,
        RichBatchErrorCode::ResourceNotFound
    );
    assert_eq!(response.items[2].status, RichBatchStatus::Ok);
    let delta = meter_delta(&reader.read_metrics()?, &before)?;
    assert_eq!(delta.cache_hits, 0);
    assert_eq!(delta.cache_misses, 0);
    assert_eq!(delta.logical_reader_bytes, 0);
    Ok(())
}

#[test]
fn artifact_corruption_marks_attempted_item_error_and_remaining_not_executed() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    publish(&root, &["needle\n"])?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = reader.manifest()?.documents[0].pages[0].native_text.clone();
    let path = root
        .join(".gptgrep/generations")
        .join(&reader.pin().generation)
        .join(&artifact.relative_path);
    fs::write(path, "damage\n")?;
    let failed = dispatch_rich_batch_with_reader(
        &reader,
        request(json!([
            {"id":"attempted","op":"read_artifact","artifact_id":artifact.id,"max_bytes":4},
            {"id":"later","op":"inspect_documents"}
        ])),
    )
    .unwrap_err();
    assert_eq!(failed.error.code, RichBatchErrorCode::SharedIntegrity);
    assert_eq!(failed.items[0].status, RichBatchStatus::Error);
    assert_eq!(failed.items[1].status, RichBatchStatus::NotExecuted);
    assert!(
        failed.items[0]
            .read_metrics
            .as_ref()
            .unwrap()
            .observed_rust_read_bytes
            > 0
    );
    assert_eq!(
        failed.read_metrics.as_ref().unwrap().logical_reader_bytes,
        0
    );
    assert!(failed.items[1].read_metrics.is_none());
    Ok(())
}
