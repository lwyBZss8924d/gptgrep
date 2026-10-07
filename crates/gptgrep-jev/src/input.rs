//! Typed evidence admission. Image transport is deliberately unqualified.
use crate::{
    JevClient, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PreparedDecision, execution::valid_digest,
};
use anyhow::{Result, anyhow, ensure};
use image::{ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
};

pub const MAX_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BASE64_BYTES: usize = 1_398_104;
pub const MAX_IMAGE_PIXELS: u64 = 8_000_000;
pub const MAX_TOTAL_IMAGE_PIXELS: u64 = 24_000_000;
pub const MAX_IMAGE_PIXEL_BUFFER_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_MULTIMODAL_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// Stable source identifiers and the digest of the originating raw source.
/// This is provenance binding, not a claim that the source was independently verified.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceBinding {
    pub source_id: String,
    pub unit_id: String,
    pub source_sha256: String,
}
impl SourceBinding {
    fn validate(&self) -> Result<()> {
        ensure!(
            [&self.source_id, &self.unit_id]
                .iter()
                .all(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)),
            "Invalid Decisions source binding"
        );
        ensure!(
            valid_digest(&self.source_sha256),
            "Invalid Decisions source digest"
        );
        Ok(())
    }
}
#[derive(Clone)]
pub struct TextEvidence {
    pub source: SourceBinding,
    pub text: String,
}
/// No Debug or serialization: compressed image bytes stay private to admission.
#[derive(Clone)]
pub struct ImageEvidence {
    source: SourceBinding,
    bytes: Vec<u8>,
    sha256: String,
    width: u32,
    height: u32,
    media_type: &'static str,
}
impl ImageEvidence {
    pub fn from_bytes(source: SourceBinding, bytes: Vec<u8>) -> Result<Self> {
        source.validate()?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES,
            "Decisions image exceeded the file byte limit"
        );
        ensure!(
            bytes.len().div_ceil(3) * 4 <= MAX_IMAGE_BASE64_BYTES,
            "Decisions image exceeded the base64 byte limit"
        );
        let format =
            image::guess_format(&bytes).map_err(|_| anyhow!("Invalid Decisions image encoding"))?;
        let media_type = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            _ => return Err(anyhow!("Decisions images require PNG or JPEG")),
        };
        let (width, height) = ImageReader::with_format(Cursor::new(&bytes), format)
            .into_dimensions()
            .map_err(|_| anyhow!("Invalid Decisions image dimensions"))?;
        let pixels = u64::from(width) * u64::from(height);
        ensure!(
            pixels > 0 && pixels <= MAX_IMAGE_PIXELS,
            "Decisions image exceeded the pixel limit"
        );
        let mut limits = Limits::default();
        limits.max_image_width = Some(width);
        limits.max_image_height = Some(height);
        limits.max_alloc = Some(MAX_IMAGE_PIXEL_BUFFER_BYTES);
        let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
        reader.limits(limits);
        let decoded = reader
            .decode()
            .map_err(|_| anyhow!("Invalid Decisions image or decoded allocation limit exceeded"))?;
        ensure!(
            decoded.as_bytes().len() as u64 <= MAX_IMAGE_PIXEL_BUFFER_BYTES,
            "Decisions image exceeded the pixel buffer limit"
        );
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        Ok(Self {
            source,
            bytes,
            sha256,
            width,
            height,
            media_type,
        })
    }
    /// Open once and read a bounded snapshot; admission and hashing use those exact bytes.
    pub fn from_file(source: SourceBinding, path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path).map_err(|_| anyhow!("Could not open Decisions image"))?;
        ensure!(
            file.metadata()
                .map_err(|_| anyhow!("Could not inspect Decisions image"))?
                .is_file(),
            "Decisions image must be a file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("Could not read Decisions image"))?;
        Self::from_bytes(source, bytes)
    }
    pub fn source(&self) -> &SourceBinding {
        &self.source
    }
    pub fn body_sha256(&self) -> &str {
        &self.sha256
    }
    pub fn file_bytes(&self) -> usize {
        self.bytes.len()
    }
    pub fn base64_bytes(&self) -> usize {
        self.bytes.len().div_ceil(3) * 4
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn media_type(&self) -> &'static str {
        self.media_type
    }
}
#[derive(Clone, Default)]
pub struct DecisionInput {
    pub text: Vec<TextEvidence>,
    pub images: Vec<ImageEvidence>,
}
impl DecisionInput {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.text.is_empty() || !self.images.is_empty(),
            "Decisions evidence input is empty"
        );
        ensure!(
            self.images.len() <= MAX_IMAGES,
            "Decisions image count exceeded the limit"
        );
        ensure!(
            self.images
                .iter()
                .map(|i| u64::from(i.width) * u64::from(i.height))
                .sum::<u64>()
                <= MAX_TOTAL_IMAGE_PIXELS,
            "Decisions images exceeded the aggregate pixel limit"
        );
        let mut ids = std::collections::BTreeSet::new();
        let mut source_digests = std::collections::BTreeMap::new();
        let mut text_bytes = 0usize;
        for item in &self.text {
            item.source.validate()?;
            if let Some(previous) =
                source_digests.insert(&item.source.source_id, &item.source.source_sha256)
            {
                ensure!(
                    previous == &item.source.source_sha256,
                    "Decisions source identifier has conflicting source digests"
                );
            }
            ensure!(
                ids.insert((&item.source.source_id, &item.source.unit_id)),
                "Duplicate Decisions source/unit binding"
            );
            ensure!(
                !item.text.trim().is_empty(),
                "Decisions text evidence is empty"
            );
            text_bytes = text_bytes.saturating_add(item.text.len());
        }
        for image in &self.images {
            image.source.validate()?;
            if let Some(previous) =
                source_digests.insert(&image.source.source_id, &image.source.source_sha256)
            {
                ensure!(
                    previous == &image.source.source_sha256,
                    "Decisions source identifier has conflicting source digests"
                );
            }
            ensure!(
                ids.insert((&image.source.source_id, &image.source.unit_id)),
                "Duplicate Decisions source/unit binding"
            );
        }
        ensure!(
            text_bytes <= MAX_REQUEST_BYTES,
            "Decisions text exceeded the byte limit"
        );
        let encoded_images = self
            .images
            .iter()
            .map(ImageEvidence::base64_bytes)
            .sum::<usize>();
        ensure!(
            encoded_images.saturating_add(text_bytes) <= MAX_MULTIMODAL_REQUEST_BYTES,
            "Decisions multimodal input exceeded the proposed byte limit"
        );
        Ok(())
    }
}
impl JevClient {
    /// Admit typed sources and reserve sufficient response capacity before paid admission.
    /// Images cannot become a prepared request until the exact Decisions codec is qualified.
    pub fn prepare_input(
        &self,
        input: DecisionInput,
        questions: Value,
        output_capacity: usize,
    ) -> Result<PreparedDecision> {
        input.validate()?;
        ensure!(
            output_capacity >= MAX_RESPONSE_BYTES,
            "Decisions output capacity must reserve the full response byte limit"
        );
        crate::validation::request(&json!("typed evidence"), &questions)?;
        ensure!(
            input.images.is_empty(),
            "Decisions image wire codec and refusal contract are unqualified; no request was prepared or submitted"
        );
        let state = json!({"evidence": input.text.iter().map(|item| json!({
            "source": item.source, "text": item.text,
            "content_sha256": format!("{:x}", Sha256::digest(item.text.as_bytes()))
        })).collect::<Vec<_>>()});
        self.prepare_decision(state, questions)
    }
}
