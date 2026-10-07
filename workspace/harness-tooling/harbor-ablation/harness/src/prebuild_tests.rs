//! MOCK raw/non-native DTO fixtures through the genuine Writer and Reader.
//! No native PDFium, OCR inference, benchmark material or provider is executed.
use super::*;
use gptgrep_parse::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

async fn candidate(root: &Path, texts: &[&str]) -> Result<(tempfile::TempDir, RichParsedDocument)> {
    let staging = tempfile::tempdir()?;
    let staging_path = staging.path().canonicalize()?;
    let source = format!(
        "synthetic source bytes for artifact custody tests: {}",
        texts.join("")
    );
    let source = source.as_bytes();
    fs::write(root.join("source.pdf"), source)?;
    fs::write(staging_path.join("prepared.pdf"), source)?;
    let sha = digest(source);
    let profile = digest(b"synthetic parser recipe");
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut create =
        |kind: ArtifactKind, page: Option<u32>, bytes: &[u8], name: &str| -> Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &digest(bytes))?,
                kind,
                relative_path: name.into(),
                sha256: digest(bytes),
                profile_sha256: profile.clone(),
                bytes: bytes.len() as u64,
                mime: if name.ends_with(".png") {
                    "image/png"
                } else if name.ends_with(".txt") {
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
        let mut pixels = Vec::new();
        use image::{ExtendedColorType, ImageEncoder};
        image::codecs::png::PngEncoder::new(&mut pixels).write_image(
            &[255; 8 * 16 * 4],
            8,
            16,
            ExtendedColorType::Rgba8,
        )?;
        let rendition = create(
            ArtifactKind::Rendition,
            Some(number),
            &pixels,
            &format!("p{number}.png"),
        )?;
        let image = OcrImage {
            artifact: rendition.clone(),
            path: staging_path.join(&rendition.relative_path),
            width: 8,
            height: 16,
            page_to_raster: AffineTransform {
                a: 0.08,
                d: 0.08,
                ..AffineTransform::IDENTITY
            },
        };
        pages.push(NativePage {
            number,
            label: FeatureState::Absent,
            width_points: 100.0,
            height_points: 200.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![image],
            complexity,
            coverage: CoverageState::Native,
            capabilities,
        });
    }
    ensure!(texts.len() == 1, "one-page ordinary host fixture");
    fs::write(staging_path.join("fixture.txt"), texts[0])?;
    let mut document = gptgrep_parse::parse_path(&staging_path.join("fixture.txt")).await?;
    document.title = "synthetic".into();
    document.parser = RICH_PARSER.into();
    create(
        ArtifactKind::MergedText,
        None,
        document.text.as_bytes(),
        "merged.txt",
    )?;
    create(ArtifactKind::Layout, None, b"[]", "layout.json")?;
    let unmapped_merged_lines = document.text.lines().count().max(1);
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
                preparation_profile_sha256: digest(&serde_json::to_vec(&(
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

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    manifest: PathBuf,
    controls: PathBuf,
    pin: GenerationPin,
    output: PathBuf,
}
fn controls() -> Controls {
    Controls {
        schema_version: CONTROLS_SCHEMA.into(),
        group_id: "ordinary-mock-group".into(),
        observation_wall_seconds: 180,
        raw_group_bytes: MAX_RAW,
        seal_inventory_bytes: 1_073_741_824,
        seal_inventory_files: MAX_INVENTORY,
        native: NativeControls {
            page_window: 25,
            dpi: 72.0,
            render_pages: true,
            max_document_pages: 25,
        },
        resource_budget: ResourceBudget::default(),
        hard_limits: HardControls {
            native_wall: false,
            native_rss: false,
            total_index_publication_bytes: false,
        },
    }
}
impl Fixture {
    async fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let base = dir.path().canonicalize()?;
        let root = base.join("raw");
        fs::create_dir(&root)?;
        let (stage, parsed) = candidate(&root, &["ordinary MOCK text\r\n中 tail\n"]).await?;
        let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
        writer.add_document("source.pdf", &stage.path().canonicalize()?, parsed)?;
        let pin = writer.publish()?;
        let source = fs::read(root.join("source.pdf"))?;
        let manifest = base.join("raw.json");
        let raw = RawManifest {
            schema_version: RAW_SCHEMA.into(),
            group_id: "ordinary-mock-group".into(),
            documents: vec![RawFile {
                path: "source.pdf".into(),
                bytes: source.len() as u64,
                sha256: digest(&source),
            }],
        };
        fs::write(&manifest, serde_json::to_vec(&raw)?)?;
        let path = base.join("controls.json");
        fs::write(&path, serde_json::to_vec(&controls())?)?;
        Ok(Self {
            _dir: dir,
            root,
            manifest,
            controls: path,
            pin,
            output: base.join("sealed"),
        })
    }
    fn inputs(&self) -> Result<Inputs> {
        Inputs::admit(&self.root, &self.manifest, &self.controls)
    }
    fn snapshot(&self) -> Result<VerifiedSnapshot> {
        VerifiedSnapshot::verify(
            self.inputs()?,
            self.pin.clone(),
            None,
            "pinned_structure_only_historical_producer_not_authenticated",
            None,
        )
    }
    fn current(&self) -> Result<Vec<u8>> {
        Ok(fs::read(self.root.join(".gptgrep/CURRENT.json"))?)
    }
    fn change_controls(&self, mutate: impl FnOnce(&mut Controls)) -> Result<()> {
        let mut c: Controls = serde_json::from_slice(&fs::read(&self.controls)?)?;
        mutate(&mut c);
        fs::write(&self.controls, serde_json::to_vec(&c)?)?;
        Ok(())
    }
}
#[tokio::test]
async fn mock_exact_pin_seals_same_admitted_buffers_and_complete_selected_tree() -> Result<()> {
    let f = Fixture::new().await?;
    let pointer = f.current()?;
    let snapshot = f.snapshot()?;
    let manifest_sha = snapshot.inputs.manifest.asset.sha256.clone();
    let controls_sha = snapshot.inputs.controls.asset.sha256.clone();
    let sha = snapshot.publish(&f.output)?;
    let bytes = fs::read(f.output.join("receipt.json"))?;
    assert_eq!(sha, digest(&bytes));
    let receipt: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(receipt["raw_manifest_sha256"], manifest_sha);
    assert_eq!(receipt["controls_sha256"], controls_sha);
    assert_eq!(receipt["current_snapshot_sha256"], digest(&pointer));
    assert_eq!(receipt["pin"]["manifest_sha256"], f.pin.manifest_sha256);
    assert!(
        receipt["producer_observation"]
            .as_str()
            .unwrap()
            .contains("not_authenticated")
    );
    assert!(receipt["native_library_identity"].is_null());
    assert_eq!(receipt["current_frontdoor_provider_calls"], 0);
    assert_eq!(
        fs::read(f.output.join("raw-manifest.json"))?,
        fs::read(&f.manifest)?
    );
    assert_eq!(
        fs::read(f.output.join("build-controls.json"))?,
        fs::read(&f.controls)?
    );
    let pinned: PinInput = serde_json::from_slice(&fs::read(f.output.join("pin.json"))?)?;
    assert_eq!(pinned.pin(), f.pin);
    assert_eq!(receipt["index_file_table_id"].as_str().unwrap().len(), 64);
    assert_eq!(receipt["index_engine_revision"].as_str().unwrap().len(), 40);
    assert_eq!(
        receipt["index_manifest_snapshot_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        receipt["inventory"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["path"].as_str().unwrap().starts_with("artifact-index/"))
            .count(),
        7
    );
    Ok(())
}
#[tokio::test]
async fn omitted_extra_and_tampered_raw_fail_without_receipt() -> Result<()> {
    for mode in 0..4 {
        let f = Fixture::new().await?;
        let pointer = f.current()?;
        match mode {
            0 => {
                fs::remove_file(f.root.join("source.pdf"))?;
            }
            1 => {
                fs::write(f.root.join("extra.pdf"), b"MOCK unlisted raw")?;
            }
            2 => {
                fs::write(f.root.join("reader-question.json"), b"MOCK unwanted input")?;
            }
            _ => {
                fs::write(f.root.join("source.pdf"), b"MOCK changed source")?;
            }
        }
        assert!(f.snapshot().is_err());
        assert!(!f.output.exists());
        assert_eq!(f.current()?, pointer);
    }
    Ok(())
}
#[tokio::test]
async fn strict_reader_gold_control_fields_paths_and_pin_are_rejected() -> Result<()> {
    let f = Fixture::new().await?;
    let original = fs::read(&f.manifest)?;
    for key in ["readers", "graders", "gold", "questions"] {
        let mut v: Value = serde_json::from_slice(&original)?;
        v[key] = json!([]);
        fs::write(&f.manifest, serde_json::to_vec(&v)?)?;
        assert!(f.inputs().is_err());
    }
    fs::write(&f.manifest, &original)?;
    let mut v: Value = serde_json::from_slice(&fs::read(&f.controls)?)?;
    v["query"] = json!("MOCK unrelated field");
    fs::write(&f.controls, serde_json::to_vec(&v)?)?;
    assert!(f.inputs().is_err());
    fs::write(&f.controls, serde_json::to_vec(&controls())?)?;
    let mut v: Value = serde_json::from_slice(&original)?;
    v["documents"][0]["path"] = json!("../source.pdf");
    fs::write(&f.manifest, serde_json::to_vec(&v)?)?;
    assert!(f.inputs().is_err());
    let timer = Timer {
        started: Instant::now(),
        lifetime: Duration::from_secs(180),
    };
    let pin = f.manifest.with_file_name("pin.json");
    fs::write(
        &pin,
        serde_json::to_vec(
            &json!({"generation":f.pin.generation,"manifest_sha256":f.pin.manifest_sha256,"gold":[]}),
        )?,
    )?;
    assert!(JsonSnapshot::<PinInput>::load(&pin, MAX_CONTROL, &timer).is_err());
    assert!(!f.output.exists());
    Ok(())
}
#[tokio::test]
async fn admitted_json_swap_is_rejected_without_replacing_original_digest() -> Result<()> {
    let f = Fixture::new().await?;
    let inputs = f.inputs()?;
    let admitted = inputs.manifest.asset.sha256.clone();
    let changed = f.manifest.with_file_name("replacement.json");
    let mut raw: Value = serde_json::from_slice(&fs::read(&f.manifest)?)?;
    raw["group_id"] = json!("other-group");
    fs::write(&changed, serde_json::to_vec(&raw)?)?;
    fs::rename(&changed, &f.manifest)?;
    assert!(inputs.fresh().is_err());
    assert_eq!(inputs.manifest.asset.sha256, admitted);
    assert!(VerifiedSnapshot::verify(inputs, f.pin.clone(), None, "MOCK", None).is_err());
    assert!(!f.output.exists());
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn leaf_and_ancestor_symlinks_and_control_corpus_overlap_are_rejected() -> Result<()> {
    use std::os::unix::fs::symlink;
    let f = Fixture::new().await?;
    let raw = f.root.join("source.pdf");
    let moved = f.manifest.with_file_name("outside.pdf");
    fs::rename(&raw, &moved)?;
    symlink(&moved, &raw)?;
    assert!(f.inputs().is_err());
    fs::remove_file(&raw)?;
    fs::rename(&moved, &raw)?;
    let alias = f.manifest.with_file_name("aliased-parent");
    symlink(f.manifest.parent().unwrap(), &alias)?;
    assert!(Inputs::admit(&f.root, &alias.join("raw.json"), &f.controls).is_err());
    assert!(Inputs::admit(&f.root, &f.root.join("raw.json"), &f.controls).is_err());
    let original_manifest = fs::read(&f.manifest)?;
    let mut manifest: Value = serde_json::from_slice(&original_manifest)?;
    let mut aliased = manifest["documents"][0].clone();
    aliased["path"] = json!("z-source.pdf");
    manifest["documents"].as_array_mut().unwrap().push(aliased);
    fs::hard_link(&raw, f.root.join("z-source.pdf"))?;
    fs::write(&f.manifest, serde_json::to_vec(&manifest)?)?;
    assert!(format!("{:#}", f.inputs().err().unwrap()).contains("raw_source_alias_rejected"));
    fs::remove_file(f.root.join("z-source.pdf"))?;
    fs::write(&f.manifest, original_manifest)?;
    let snapshot = f.snapshot()?;
    assert!(snapshot.publish(&f.root.join("sealed")).is_err());
    assert!(!f.root.join("sealed").exists());
    Ok(())
}
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        } else {
            fs::copy(e.path(), to.join(e.file_name()))?;
        }
    }
    Ok(())
}
#[tokio::test]
async fn copied_canonical_root_cannot_rewrite_manifest_identity() -> Result<()> {
    let f = Fixture::new().await?;
    let copied = f.manifest.with_file_name("copied-raw");
    copy_tree(&f.root, &copied)?;
    let inputs = Inputs::admit(&copied, &f.manifest, &f.controls)?;
    assert!(VerifiedSnapshot::verify(inputs, f.pin.clone(), None, "MOCK", None).is_err());
    assert!(!f.output.exists());
    Ok(())
}
#[tokio::test]
async fn source_prepared_native_rendition_tree_and_index_tamper_refuse_seal() -> Result<()> {
    for target in ["prepared", "native", "rendition", "tree", "index"] {
        let f = Fixture::new().await?;
        let dir = f.root.join(".gptgrep/generations").join(&f.pin.generation);
        let reader = RichGenerationReader::open_pinned(&f.root, &f.pin)?;
        let doc = &reader.manifest()?.documents[0];
        let path = match target {
            "prepared" => dir.join(&doc.prepared_path),
            "native" => dir.join(&doc.pages[0].native_text.relative_path),
            "rendition" => dir.join(&doc.pages[0].renditions[0].artifact.relative_path),
            "tree" => dir.join(format!("text/{}.txt", doc.document.id)),
            _ => dir.join("artifact-index/meta.json"),
        };
        drop(reader);
        let mut bytes = fs::read(&path)?;
        bytes[0] ^= 1;
        fs::write(&path, &bytes)?;
        assert!(f.snapshot().is_err(), "target {target}");
        assert!(!f.output.exists());
    }
    Ok(())
}
#[tokio::test]
async fn changed_current_refuses_receipt_even_when_retained_pin_is_valid() -> Result<()> {
    let f = Fixture::new().await?;
    let snapshot = f.snapshot()?;
    let (stage, parsed) = candidate(&f.root, &["ordinary MOCK text\r\n中 tail\n"]).await?;
    let mut writer = RichGenerationWriter::begin(&f.root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &stage.path().canonicalize()?, parsed)?;
    writer.publish()?;
    RichGenerationReader::open_pinned(&f.root, &f.pin)?.verify_all()?;
    assert!(snapshot.publish(&f.output).is_err());
    assert!(!f.output.exists());
    Ok(())
}
#[tokio::test]
async fn missing_ocr_or_capability_blocker_preserves_current_before_publish() -> Result<()> {
    let f = Fixture::new().await?;
    let pointer = f.current()?;
    for coverage in [CoverageState::RequiredOcr, CoverageState::ValidatedOcr] {
        assert!(native_coverage(&[(1, coverage)], &[]).is_err());
    }
    assert!(
        native_coverage(
            &[(1, CoverageState::Native)],
            &["MOCK unsupported capability".into()]
        )
        .is_err()
    );
    let (stage, mut parsed) = candidate(&f.root, &["MOCK text"]).await?;
    parsed.pages[0].coverage = CoverageState::RequiredOcr;
    parsed.coverage[0].1 = CoverageState::RequiredOcr;
    let mut writer = RichGenerationWriter::begin(&f.root, ResourceBudget::default())?;
    assert!(
        writer
            .add_document("source.pdf", &stage.path().canonicalize()?, parsed)
            .is_err()
    );
    assert!(writer.publish().is_err());
    assert_eq!(f.current()?, pointer);
    Ok(())
}
#[tokio::test]
async fn unsupported_hard_controls_and_raw_bytes_fail_before_native_writer() -> Result<()> {
    let f = Fixture::new().await?;
    let pointer = f.current()?;
    for field in 0..3 {
        f.change_controls(|c| {
            c.hard_limits = HardControls {
                native_wall: field == 0,
                native_rss: field == 1,
                total_index_publication_bytes: field == 2,
            };
        })?;
        assert!(f.inputs().is_err());
        assert_eq!(f.current()?, pointer);
    }
    f.change_controls(|c| {
        c.hard_limits = HardControls {
            native_wall: false,
            native_rss: false,
            total_index_publication_bytes: false,
        };
        c.raw_group_bytes = 1;
    })?;
    assert!(f.inputs().is_err());
    assert_eq!(f.current()?, pointer);
    assert!(!f.output.exists());
    Ok(())
}
#[tokio::test]
async fn expired_admission_and_seal_bytes_refuse_output_and_keep_current() -> Result<()> {
    let f = Fixture::new().await?;
    let pointer = f.current()?;
    let mut inputs = f.inputs()?;
    inputs.timer.lifetime = Duration::ZERO;
    assert!(build_native(inputs, &f.output).await.is_err());
    assert_eq!(f.current()?, pointer);
    f.change_controls(|c| c.seal_inventory_bytes = 1)?;
    assert!(f.snapshot().is_err());
    assert_eq!(f.current()?, pointer);
    assert!(!f.output.exists());
    assert!(bounded_json(&"x".repeat(MAX_RECEIPT + 1)).is_err());
    Ok(())
}
#[tokio::test]
async fn fresh_output_collision_is_nonmutating_and_selected_history_scope_is_explicit() -> Result<()>
{
    let f = Fixture::new().await?;
    let sibling = f.root.join(".gptgrep/generations/unselected-MOCK-history");
    fs::create_dir(&sibling)?;
    fs::write(sibling.join("unverified.bin"), b"MOCK sibling bytes")?;
    let snapshot = f.snapshot()?;
    assert!(
        snapshot
            .receipt
            .inventory_scope
            .contains("sibling_history_unverified")
    );
    fs::create_dir(&f.output)?;
    fs::write(f.output.join("owned-marker"), b"retain")?;
    assert!(snapshot.publish(&f.output).is_err());
    assert_eq!(fs::read(f.output.join("owned-marker"))?, b"retain");
    assert!(!f.output.join("receipt.json").exists());
    Ok(())
}
#[tokio::test]
async fn retained_snapshot_rechecks_sources_and_extra_selected_generation_files() -> Result<()> {
    for raw in [false, true] {
        let f = Fixture::new().await?;
        let snapshot = f.snapshot()?;
        if raw {
            fs::write(
                f.root.join("source.pdf"),
                b"MOCK source changed after validation",
            )?;
        } else {
            fs::write(
                f.root
                    .join(".gptgrep/generations")
                    .join(&f.pin.generation)
                    .join("unrelated-reader.json"),
                b"MOCK unlisted metadata",
            )?;
        }
        assert!(snapshot.publish(&f.output).is_err());
        assert!(!f.output.exists());
    }
    Ok(())
}

#[tokio::test]
async fn selected_shared_staging_failure_keeps_current_and_does_not_reset_counter() -> Result<()> {
    let f = Fixture::new().await?;
    let pointer = f.current()?;
    let (stage, parsed) = candidate(&f.root, &["ordinary MOCK text\r\n中 tail\n"]).await?;
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 1;
    let mut writer = RichGenerationWriter::begin(&f.root, budget.clone())?;
    assert!(
        writer
            .add_document("source.pdf", &stage.path().canonicalize()?, parsed)
            .is_err()
    );
    assert!(writer.publish().is_err());
    assert_eq!(f.current()?, pointer);
    // A failed reservation does not mint a fresh invocation or increase capacity.
    budget.reserve_retained_staging(1)?;
    assert!(budget.clone().reserve_retained_staging(1).is_err());
    assert_eq!(budget.staging_used(), 1);
    Ok(())
}
