use super::*;
use crate::{RichGenerationReader, RichGenerationWriter};
use gptgrep_pageindex::{PageInput, from_pages};
use gptgrep_parse::{
    AffineTransform, ArtifactKind, ArtifactRef, ConversionIdentity, CoverageState, FeatureState,
    NativePage, OcrImage, PageGeometry, PreparedSummary, RICH_PARSER, RichParsedDocument,
};
use image::ImageEncoder;
use serde_json::json;
use std::{fs, path::PathBuf};

// Ordinary public writer/reader fixtures use synthetic parser DTOs and a valid
// PNG. They do not claim native PDF extraction or model/image delivery utility.
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    staging: tempfile::TempDir,
    parsed: RichParsedDocument,
    reader: VisualGenerationReader,
    id: String,
    artifact_path: PathBuf,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let stage = staging.path().canonicalize().unwrap();
    let source = b"synthetic derived-cache source";
    fs::write(root.join("source.pdf"), source).unwrap();
    fs::write(stage.join("prepared.pdf"), source).unwrap();
    let source_sha = hash(source);
    let profile = hash(b"synthetic owned PNG profile");
    let rgba = (0u8..6)
        .flat_map(|y| (0u8..8).flat_map(move |x| [x * 21, y * 33, 44, 255]))
        .collect::<Vec<_>>();
    let mut png = vec![];
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Uncompressed,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(&rgba, 8, 6, image::ExtendedColorType::Rgba8)
    .unwrap();
    let mut artifacts = vec![];
    let mut artifact = |kind: ArtifactKind,
                        page: Option<u32>,
                        data: &[u8],
                        name: &str,
                        mime: &str| {
        let a = ArtifactRef {
            id: gptgrep_parse::artifact_id(&source_sha, &profile, &kind, page, None, &hash(data))
                .unwrap(),
            kind,
            relative_path: name.into(),
            sha256: hash(data),
            profile_sha256: profile.clone(),
            bytes: data.len() as u64,
            mime: mime.into(),
            page,
            region_id: None,
        };
        fs::write(stage.join(name), data).unwrap();
        artifacts.push(a.clone());
        a
    };
    let native_text = artifact(
        ArtifactKind::NativeText,
        Some(1),
        b"fixture text",
        "native.txt",
        "text/plain; charset=utf-8",
    );
    let geometry = PageGeometry {
        width_points: 8.0,
        height_points: 6.0,
        rotation_quarter_turns: 0,
        user_unit: 1.0,
        source_to_page: AffineTransform::IDENTITY,
        page_to_source: AffineTransform::IDENTITY,
    };
    let complexity = serde_json::from_value(json!({"page_number":1,"text_length":12,"text_coverage":0.5,"has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,"full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":48.0,"needs_ocr":false,"reasons":[],"layout":null})).unwrap();
    let capabilities =
        BTreeMap::from([("annotation_count".into(), FeatureState::Present("0".into()))]);
    let metadata = serde_json::to_vec(&json!({"page":{"page_number":1,"page_width":8.0,"page_height":6.0,"text_items":[]},"objects":[],"capabilities":capabilities,"complexity":complexity,"geometry":geometry,"image_rects":[]})).unwrap();
    let metadata_artifact = artifact(
        ArtifactKind::NativeMetadata,
        Some(1),
        &metadata,
        "native.json",
        "application/json",
    );
    let rendition = artifact(
        ArtifactKind::Rendition,
        Some(1),
        &png,
        "page.png",
        "image/png",
    );
    let document = from_pages(
        "synthetic",
        &[PageInput {
            number: 1,
            text: "fixture text".into(),
        }],
        &[],
        &[],
        RICH_PARSER,
    )
    .unwrap();
    artifact(
        ArtifactKind::MergedText,
        None,
        document.text.as_bytes(),
        "merged.txt",
        "text/plain; charset=utf-8",
    );
    artifact(
        ArtifactKind::Layout,
        None,
        b"[]",
        "layout.json",
        "application/json",
    );
    let parsed = RichParsedDocument {
        summary: PreparedSummary {
            source_sha256: source_sha.clone(),
            prepared_sha256: source_sha,
            source_bytes: source.len() as u64,
            prepared_bytes: source.len() as u64,
            format: "pdf".into(),
            parser_identity: RICH_PARSER.into(),
            preparation_profile_sha256: hash_json(&(
                "gptgrep.prepare.identity-pdf.v1",
                gptgrep_parse::LITEPARSE_REVISION,
            ))
            .unwrap(),
            conversion: FeatureState::Present(ConversionIdentity {
                converter: "identity-pdf".into(),
                version: gptgrep_parse::LITEPARSE_REVISION.into(),
                invocations: 0,
            }),
            image_geometry: FeatureState::Absent,
        },
        document,
        pages: vec![NativePage {
            number: 1,
            label: FeatureState::Absent,
            width_points: 8.0,
            height_points: 6.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![OcrImage {
                artifact: rendition,
                path: PathBuf::new(),
                width: 8,
                height: 6,
                page_to_raster: AffineTransform::IDENTITY,
            }],
            complexity,
            coverage: CoverageState::Native,
            capabilities,
        }],
        artifacts,
        segments: vec![],
        ocr_results: vec![],
        ocr_tasks: vec![],
        native_profile_sha256: profile,
        coverage: vec![(1, CoverageState::Native)],
        unmapped_merged_lines: 1,
    };
    let pin = publish(&root, &stage, parsed.clone());
    let reader = RichGenerationReader::open_pinned(&root, &pin).unwrap();
    let image = &reader.manifest().unwrap().documents[0].pages[0].renditions[0];
    let id = image.artifact.id.clone();
    let artifact_path = root
        .join(".gptgrep")
        .join("generations")
        .join(&pin.generation)
        .join(&image.artifact.relative_path);
    let reader = reader.visual_reader().unwrap();
    Fixture {
        _temp: temp,
        root,
        staging,
        parsed,
        reader,
        id,
        artifact_path,
    }
}
fn publish(
    root: &std::path::Path,
    staging: &std::path::Path,
    parsed: RichParsedDocument,
) -> GenerationPin {
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default()).unwrap();
    writer.add_document("source.pdf", staging, parsed).unwrap();
    writer.publish().unwrap()
}
impl Fixture {
    fn parent(&self) -> VerifiedRenditionFile {
        self.reader
            .open_rendition(&self.id, &ResourceBudget::default())
            .unwrap()
    }
}
fn selected() -> BoundingBox {
    BoundingBox {
        x: 2.0,
        y: 1.0,
        width: 3.0,
        height: 3.0,
    }
}
fn other() -> BoundingBox {
    BoundingBox {
        x: 0.0,
        y: 0.0,
        width: 3.0,
        height: 3.0,
    }
}
fn cache() -> DerivedVisualCache {
    DerivedVisualCache::new(DerivedCacheBudget::default()).unwrap()
}

#[test]
fn derives_exact_pixels_and_typed_recipe_without_mutating_generation() {
    let f = fixture();
    let cache = cache();
    let budget = ResourceBudget::default();
    let current = fs::read(f.root.join(".gptgrep/CURRENT.json")).unwrap();
    let manifest = fs::read(
        f.artifact_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("manifest.json"),
    )
    .unwrap();
    let pin = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    assert!(pin.crop_executed() && !pin.cache_hit());
    let r = pin.receipt();
    assert_eq!(r.origin, VisualSource::Derived);
    assert_eq!(r.parent_origin, VisualSource::Manifest);
    assert!(!r.actual_image_delivery);
    assert_eq!((r.width, r.height), (3, 3));
    assert_eq!(
        r.output_sha256,
        hash(pin.read(&budget, MAX_RESPONSE).unwrap())
    );
    let image = image::load_from_memory(pin.read(&budget, MAX_RESPONSE).unwrap())
        .unwrap()
        .to_rgba8();
    for y in 0..3 {
        for x in 0..3 {
            assert_eq!(
                image.get_pixel(x, y).0,
                [((x + 2) * 21) as u8, ((y + 1) * 33) as u8, 44, 255]
            );
        }
    }
    assert_eq!(r.recipe.page_to_crop.apply(2.0, 1.0), (0.0, 0.0));
    assert_eq!(r.recipe.crop_to_page.apply(3.0, 3.0), (5.0, 4.0));
    assert!(pin.envelope_upper_bytes() <= MAX_RESPONSE as u64);
    assert_eq!(
        current,
        fs::read(f.root.join(".gptgrep/CURRENT.json")).unwrap()
    );
    assert_eq!(
        manifest,
        fs::read(
            f.artifact_path
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("manifest.json")
        )
        .unwrap()
    );
}

#[test]
fn hits_and_reads_revalidate_the_owned_parent_without_redecoding() {
    let f = fixture();
    let cache = cache();
    let budget = ResourceBudget::default();
    let first = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    let hit = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    assert!(hit.cache_hit() && !hit.crop_executed());
    assert_eq!(
        first.read(&budget, MAX_RESPONSE).unwrap(),
        hit.read(&budget, MAX_RESPONSE).unwrap()
    );
    let before_change = f.parent();
    fs::write(f.root.join("source.pdf"), b"changed source").unwrap();
    assert!(hit.read(&budget, MAX_RESPONSE).is_err());
    assert!(
        cache
            .resume(before_change, &first.recipe_token(), &budget, MAX_RESPONSE)
            .is_err()
    );
    assert_eq!(cache.status().unwrap().working_reserved_bytes, 0);
}

#[test]
fn pins_tokens_clones_and_clear_keep_their_original_capacity_charges() {
    let f = fixture();
    let budget = ResourceBudget::default();
    let cache = DerivedVisualCache::new(DerivedCacheBudget {
        entries: 1,
        metadata_bytes: METADATA_CHARGE,
        ..DerivedCacheBudget::default()
    })
    .unwrap();
    let pin = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    let clone = pin.clone();
    let token = pin.recipe_token();
    let cache_clone = cache.clone();
    cache.clear().unwrap();
    let state = cache_clone.status().unwrap();
    assert_eq!(state.payload_resident_bytes, 0);
    assert!(state.payload_retained_bytes > 0);
    assert_eq!(state.metadata_reserved_bytes, METADATA_CHARGE);
    assert!(
        cache
            .derive(f.parent(), other(), &budget, MAX_RESPONSE)
            .is_err()
    );
    drop(pin);
    drop(clone);
    assert_eq!(cache.status().unwrap().payload_retained_bytes, 0);
    assert_eq!(
        cache.status().unwrap().metadata_reserved_bytes,
        METADATA_CHARGE
    );
    assert!(
        cache
            .derive(f.parent(), other(), &budget, MAX_RESPONSE)
            .is_err()
    );
    drop(token);
    assert_eq!(cache.status().unwrap().metadata_reserved_bytes, 0);
    assert!(
        cache_clone
            .derive(f.parent(), other(), &budget, MAX_RESPONSE)
            .is_ok()
    );
}

#[test]
fn lru_eviction_and_recipe_only_resume_rebuild_identical_same_pin_bytes() {
    let f = fixture();
    let budget = ResourceBudget::default();
    let probe = cache()
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    let bytes = probe.receipt().encoded_bytes as usize;
    let expected = probe.receipt().clone();
    let cache = DerivedVisualCache::new(DerivedCacheBudget {
        payload_bytes: bytes,
        entries: 2,
        metadata_bytes: METADATA_CHARGE * 2,
        ..DerivedCacheBudget::default()
    })
    .unwrap();
    let first = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    let token = first.recipe_token();
    drop(first);
    let second = cache
        .derive(f.parent(), other(), &budget, MAX_RESPONSE)
        .unwrap();
    drop(second);
    assert_eq!(cache.status().unwrap().payload_resident_entries, 1);
    let rebuilt = cache
        .resume(f.parent(), &token, &budget, MAX_RESPONSE)
        .unwrap();
    assert!(rebuilt.crop_executed() && !rebuilt.cache_hit());
    assert_eq!(rebuilt.receipt().derivative_id, expected.derivative_id);
    assert_eq!(
        hash(rebuilt.read(&budget, MAX_RESPONSE).unwrap()),
        expected.output_sha256
    );
    drop(rebuilt);
    cache.clear().unwrap();
    let portable = cache
        .rebuild(f.parent(), &expected, &budget, MAX_RESPONSE)
        .unwrap();
    assert_eq!(
        hash_json(portable.receipt()).unwrap(),
        hash_json(&expected).unwrap()
    );
}

#[test]
fn portable_receipts_cannot_forge_lineage_transforms_output_or_new_generation() {
    let f = fixture();
    let cache = cache();
    let budget = ResourceBudget::default();
    let pin = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    for kind in 0..4 {
        let mut forged = pin.receipt().clone();
        match kind {
            0 => forged.recipe.crop_to_page.e += 1.0,
            1 => forged.output_sha256 = hash(b"forged"),
            2 => forged.parent_binding_sha256 = hash(b"unrelated parent"),
            _ => forged.actual_image_delivery = true,
        }
        assert!(
            cache
                .rebuild(f.parent(), &forged, &budget, MAX_RESPONSE)
                .is_err()
        );
    }
    let expected = pin.receipt().clone();
    let new_pin = publish(
        &f.root,
        &f.staging.path().canonicalize().unwrap(),
        f.parsed.clone(),
    );
    let new_reader = RichGenerationReader::open_pinned(&f.root, &new_pin)
        .unwrap()
        .visual_reader()
        .unwrap();
    assert!(
        cache
            .rebuild(
                new_reader.open_rendition(&f.id, &budget).unwrap(),
                &expected,
                &budget,
                MAX_RESPONSE
            )
            .is_err()
    );
    assert!(pin.read(&budget, MAX_RESPONSE).is_ok());
}

#[test]
fn changed_parent_artifact_and_corrupted_metadata_are_rejected() {
    let f = fixture();
    let cache = cache();
    let budget = ResourceBudget::default();
    let mut pin = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    let original = fs::read(&f.artifact_path).unwrap();
    let mut changed = original.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    fs::write(&f.artifact_path, changed).unwrap();
    let error = pin.read(&budget, MAX_RESPONSE).unwrap_err().to_string();
    assert!(!error.contains(f.root.to_str().unwrap()));
    fs::write(&f.artifact_path, original).unwrap();
    cache.clear().unwrap();
    Arc::get_mut(&mut pin.metadata)
        .unwrap()
        .receipt
        .output_sha256 = hash(b"corruption");
    assert_eq!(
        pin.read(&budget, MAX_RESPONSE).unwrap_err().to_string(),
        "derived_receipt_changed"
    );
}

#[test]
fn response_pixel_working_and_payload_capacity_fail_without_retained_copies() {
    let f = fixture();
    let budget = ResourceBudget::default();
    let cache = cache();
    assert!(
        cache
            .derive(
                f.parent(),
                selected(),
                &budget,
                MAX_METADATA + WIRE_OVERHEAD
            )
            .is_err()
    );
    assert!(
        cache
            .derive(
                f.parent(),
                selected(),
                &budget,
                MAX_METADATA + WIRE_OVERHEAD + 4
            )
            .is_err()
    );
    let invalid = BoundingBox {
        width: 0.0,
        ..selected()
    };
    assert!(
        cache
            .derive(f.parent(), invalid, &budget, MAX_RESPONSE)
            .is_err()
    );
    let mut decoder = budget.clone();
    decoder.image_pixel_buffer_bytes = 16_500;
    assert!(
        cache
            .derive(f.parent(), selected(), &decoder, MAX_RESPONSE)
            .is_err()
    );
    assert_eq!(cache.status().unwrap().metadata_reserved_bytes, 0);
    assert_eq!(cache.status().unwrap().working_reserved_bytes, 0);
    let tight = DerivedVisualCache::new(DerivedCacheBudget {
        payload_bytes: 1,
        ..DerivedCacheBudget::default()
    })
    .unwrap();
    assert!(
        tight
            .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
            .is_err()
    );
    assert_eq!(tight.status().unwrap().payload_retained_bytes, 0);
    tight.clear().unwrap();
    assert_eq!(tight.status().unwrap().metadata_reserved_bytes, 0);
}

#[test]
fn resumed_decode_counts_its_already_allocated_cache_fill_buffer() {
    let f = fixture();
    let probe = cache()
        .derive(
            f.parent(),
            selected(),
            &ResourceBudget::default(),
            MAX_RESPONSE,
        )
        .unwrap();
    let mut resources = ResourceBudget::default();
    resources.owned_working_bytes =
        probe.receipt().admission.owned_working_upper_bytes + TRANSIENT_WORKING + 1;
    let cache = cache();
    let initial = cache
        .derive(f.parent(), selected(), &resources, MAX_RESPONSE)
        .unwrap();
    let token = initial.recipe_token();
    assert!(initial.read(&resources, MAX_RESPONSE).is_ok());
    let second = cache
        .derive(f.parent(), selected(), &resources, MAX_RESPONSE)
        .unwrap();
    assert!(second.cache_hit() && !second.crop_executed());
    assert!(second.read(&resources, MAX_RESPONSE).is_ok());
    let resumed_hit = cache
        .resume(f.parent(), &token, &resources, MAX_RESPONSE)
        .unwrap();
    assert!(resumed_hit.cache_hit() && !resumed_hit.crop_executed());
    assert!(resumed_hit.read(&resources, MAX_RESPONSE).is_ok());
    drop(second);
    drop(resumed_hit);
    drop(initial);
    cache.clear().unwrap();
    assert_eq!(
        cache
            .resume(f.parent(), &token, &resources, MAX_RESPONSE)
            .err()
            .unwrap()
            .to_string(),
        "derived_resume_working_rejected"
    );
    assert_eq!(cache.status().unwrap().payload_retained_bytes, 0);
    assert_eq!(cache.status().unwrap().working_reserved_bytes, 0);
}

#[test]
fn selected_profile_and_opaque_token_cache_owner_are_bound() {
    let f = fixture();
    let cache = cache();
    let budget = ResourceBudget::default();
    let pin = cache
        .derive(f.parent(), selected(), &budget, MAX_RESPONSE)
        .unwrap();
    let mut changed = budget.clone();
    changed.image_pixels -= 1;
    assert!(pin.read(&changed, MAX_RESPONSE).is_err());
    let other_cache = DerivedVisualCache::new(DerivedCacheBudget::default()).unwrap();
    assert!(
        other_cache
            .resume(f.parent(), &pin.recipe_token(), &budget, MAX_RESPONSE)
            .is_err()
    );
    let tiny = DerivedVisualCache::new(DerivedCacheBudget {
        working_bytes: TRANSIENT_WORKING + 1,
        ..DerivedCacheBudget::default()
    })
    .unwrap();
    assert!(
        tiny.derive(f.parent(), selected(), &budget, MAX_RESPONSE)
            .is_err()
    );
    assert_eq!(tiny.status().unwrap().working_reserved_bytes, 0);
}
