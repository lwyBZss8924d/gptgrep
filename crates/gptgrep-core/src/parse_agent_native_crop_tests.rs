//! Authored, UNRUN generated-image registry tests. No provider/runtime is used;
//! future execution requires admitted external temporary storage.
use super::*;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    budget: ResourceBudget,
}
impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        std::fs::create_dir(root.join("staging"))?;
        let source = root.join("generated.png");
        image::RgbaImage::from_fn(32, 24, |x, y| {
            image::Rgba([(x * 7) as u8, (y * 9) as u8, ((x + y) * 3) as u8, 255])
        })
        .save(&source)?;
        Ok(Self {
            _temporary: temporary,
            root,
            source,
            budget: ResourceBudget::default(),
        })
    }
    fn toolbox(&self) -> Result<ParseToolbox> {
        let mut toolbox = ParseToolbox::new(
            &self.root.join("staging"),
            [31; 32],
            self.budget.clone(),
            ToolboxLimits::default(),
        )?;
        let bytes = std::fs::read(&self.source)?;
        toolbox.register_source("approved", &self.source, bytes.len() as u64, &sha(&bytes))?;
        Ok(toolbox)
    }
}
async fn call(toolbox: &mut ParseToolbox, operation: ToolboxOperation) -> Result<Value> {
    Ok(serde_json::from_slice(
        &toolbox
            .dispatch(ToolboxRequest {
                schema: REQUEST_SCHEMA.into(),
                id: "crop-regression".into(),
                max_reply_bytes: 65_536,
                operation,
            })
            .await?,
    )?)
}
async fn prepare(toolbox: &mut ParseToolbox) -> Result<(String, String, String, BoundingBox)> {
    let prepared = call(
        toolbox,
        ToolboxOperation::Prepare {
            source: "approved".into(),
        },
    )
    .await?;
    let handle = prepared["data"]["handle"]
        .as_str()
        .context("synthetic handle")?
        .to_string();
    call(
        toolbox,
        ToolboxOperation::ExtractNative {
            handle: handle.clone(),
            page_window: 25,
            dpi: 96.0,
            render_pages: true,
        },
    )
    .await?;
    let doc = toolbox.doc(&handle)?;
    let task = doc
        .stage
        .tasks()?
        .first()
        .context("generated image OCR task")?;
    let parent = task.images.first().context("generated image")?;
    Ok((
        handle,
        task.task_sha256.clone(),
        parent.artifact.id.clone(),
        BoundingBox {
            x: 0.0,
            y: 0.0,
            width: task.page_width_points / 2.0,
            height: task.page_height_points / 2.0,
        },
    ))
}
struct FailingNative {
    calls: AtomicU64,
    task_sha256: String,
}
impl OcrExecutor for FailingNative {
    fn execute<'a>(
        &'a self,
        _: OcrTask,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OcrPageResult>> + Send + 'a>>
    {
        Box::pin(async { Err(anyhow::anyhow!("non-crop path not expected")) })
    }
    fn execute_with_native_crops<'a>(
        &'a self,
        task: OcrTask,
        crops: Vec<NativeOcrCrop>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OcrPageResult>> + Send + 'a>>
    {
        Box::pin(async move {
            assert_eq!(task.task_sha256, self.task_sha256);
            assert_eq!(crops.len(), 1);
            gptgrep_parse::validate_native_crop_set(&task, &crops)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("synthetic owned executor failed"))
        })
    }
}

#[tokio::test]
async fn typed_crop_preserves_registry_and_one_original_submission() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut toolbox = fixture.toolbox()?;
    let (handle, task_sha, parent, bbox) = prepare(&mut toolbox).await?;
    let doc = toolbox.doc(&handle)?;
    let count = doc.artifacts.len();
    let before = serde_json::to_vec(doc.stage.tasks()?.first().context("task")?)?;
    let crop = toolbox.create_native_crop(&handle, &parent, bbox)?;
    assert_eq!(crop.proof().task_sha256, task_sha);
    assert!(
        !toolbox
            .doc(&handle)?
            .artifacts
            .contains_key(&crop.proof().derived_artifact_id)
    );
    assert_eq!(toolbox.doc(&handle)?.artifacts.len(), count);
    assert_eq!(
        serde_json::to_vec(
            toolbox
                .doc(&handle)?
                .stage
                .tasks()?
                .first()
                .context("task")?
        )?,
        before
    );
    let executor = FailingNative {
        calls: AtomicU64::new(0),
        task_sha256: task_sha.clone(),
    };
    assert!(
        toolbox
            .execute_ocr_with_native_crops(&handle, &task_sha, vec![crop], &executor)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(toolbox.create_native_crop(&handle, &parent, bbox).is_err());
    assert!(
        toolbox
            .execute_ocr(&handle, &task_sha, &executor)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn closed_handle_and_next_source_refuse_old_crop_without_refund() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut toolbox = fixture.toolbox()?;
    let (handle, task_sha, parent, bbox) = prepare(&mut toolbox).await?;
    let crop = toolbox.create_native_crop(&handle, &parent, bbox)?;
    let retained = fixture.budget.staging_used();
    call(
        &mut toolbox,
        ToolboxOperation::Close {
            handle: handle.clone(),
        },
    )
    .await?;
    assert!(toolbox.create_native_crop(&handle, &parent, bbox).is_err());
    assert_eq!(fixture.budget.staging_used(), retained);
    let (next, next_task, _, _) = prepare(&mut toolbox).await?;
    assert_ne!(next, handle);
    // Reopening the identical source changes its handle, not its content-bound task.
    assert_eq!(next_task, task_sha);
    let executor = FailingNative {
        calls: AtomicU64::new(0),
        task_sha256: next_task.clone(),
    };
    assert!(
        toolbox
            .execute_ocr_with_native_crops(&next, &next_task, vec![crop], &executor)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.budget.staging_used() >= retained);
    Ok(())
}

#[tokio::test]
async fn changed_parent_fails_before_the_executor_and_no_unknown_parent_registers() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut toolbox = fixture.toolbox()?;
    let (handle, task_sha, parent, bbox) = prepare(&mut toolbox).await?;
    assert!(
        toolbox
            .create_native_crop(&handle, &format!("artifact-{}", "a".repeat(64)), bbox)
            .is_err()
    );
    let crop = toolbox.create_native_crop(&handle, &parent, bbox)?;
    let path = toolbox
        .doc(&handle)?
        .stage
        .tasks()?
        .first()
        .context("task")?
        .images[0]
        .path
        .clone();
    std::fs::write(path, b"changed source-derived parent")?;
    let executor = FailingNative {
        calls: AtomicU64::new(0),
        task_sha256: task_sha.clone(),
    };
    assert!(
        toolbox
            .execute_ocr_with_native_crops(&handle, &task_sha, vec![crop], &executor)
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn default_executor_refuses_native_supplement_before_legacy_execution() -> Result<()> {
    struct LegacyOnly(AtomicU64);
    impl OcrExecutor for LegacyOnly {
        fn execute<'a>(
            &'a self,
            _: OcrTask,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OcrPageResult>> + Send + 'a>>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(anyhow::anyhow!("synthetic legacy executor")) })
        }
    }
    let fixture = Fixture::new()?;
    let mut toolbox = fixture.toolbox()?;
    let (handle, task, parent, bbox) = prepare(&mut toolbox).await?;
    let crop = toolbox.create_native_crop(&handle, &parent, bbox)?;
    let legacy = LegacyOnly(AtomicU64::new(0));
    assert!(
        toolbox
            .execute_ocr_with_native_crops(&handle, &task, vec![crop], &legacy)
            .await
            .is_err()
    );
    assert_eq!(legacy.0.load(Ordering::SeqCst), 0);
    Ok(())
}
