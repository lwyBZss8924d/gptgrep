//! Typed evidence admission and an explicit, experimental image qualification route.
use crate::{
    DEFAULT_ENDPOINT, JevClient, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PreparedDecision,
    execution::valid_digest,
};
use anyhow::{Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Cursor, Read, Write},
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

/// Experimental image request with its exact admitted sources retained locally.
///
/// Preparing this request establishes syntax and local admission only. It does not
/// qualify image semantics or change the default typed image rejection. There is
/// deliberately no Debug or serde implementation for these source-bearing bytes.
pub struct PreparedImageQualification {
    request: PreparedDecision,
    input: DecisionInput,
}

impl PreparedImageQualification {
    pub fn request(&self) -> &PreparedDecision {
        &self.request
    }

    pub fn source_input(&self) -> &DecisionInput {
        &self.input
    }

    pub fn text_content_sha256(&self, index: usize) -> Option<String> {
        self.input
            .text
            .get(index)
            .map(|item| format!("{:x}", Sha256::digest(item.text.as_bytes())))
    }

    /// Consume the sidecar and retain the exact existing client-bound request.
    /// Callers that need provenance after submission must retain it separately.
    pub fn into_request(self) -> PreparedDecision {
        self.request
    }
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
    /// Prepare the documented flat OpenRouter state for an explicit qualification.
    ///
    /// Only the exact OpenRouter alpha endpoint and requested Luna Decisions model
    /// are admitted. Text is serialized literally; source metadata stays in the
    /// wrapper. PNG/JPEG bytes become direct image_url data URLs, without detail.
    /// The escaped text/questions transport envelope has a stricter 64 KiB cap;
    /// the full image-bearing body remains bounded by 8 MiB. No request is sent.
    pub fn prepare_image_qualification(
        &self,
        input: DecisionInput,
        questions: Value,
        output_capacity: usize,
    ) -> Result<PreparedImageQualification> {
        self.execution.remaining()?;
        ensure!(
            self.endpoint.as_str() == DEFAULT_ENDPOINT
                && self.model == "openai/gpt-6-luna-decisions",
            "Image qualification requires the exact OpenRouter alpha endpoint and Luna Decisions model"
        );
        ensure!(
            output_capacity >= MAX_RESPONSE_BYTES,
            "Decisions output capacity must reserve the full response byte limit"
        );
        input.validate()?;
        ensure!(
            !input.images.is_empty(),
            "Image qualification requires admitted PNG or JPEG evidence"
        );
        crate::validation::request(&Value::String("typed evidence".into()), &questions)?;

        // Count the escaped envelope before allocating base64 or request bytes.
        // This pass borrows text/questions and never visits the image encoder.
        let mut envelope = BoundedWriter::counting(MAX_REQUEST_BYTES);
        serialize_qualification(self, &input, &questions, false, &mut envelope).map_err(|_| {
            anyhow!("Image qualification escaped text/questions envelope exceeded the byte limit")
        })?;
        let upper_bound = input.images.iter().try_fold(envelope.len, |total, image| {
            total.checked_add(image.base64_bytes())?.checked_add(128)
        });
        ensure!(
            upper_bound.is_some_and(|bytes| bytes <= MAX_MULTIMODAL_REQUEST_BYTES),
            "Decisions multimodal request exceeded the whole-body byte limit"
        );
        let mut body = BoundedWriter::buffered(MAX_MULTIMODAL_REQUEST_BYTES);
        body.bytes
            .try_reserve_exact(upper_bound.expect("The bound was checked above"))
            .map_err(|_| anyhow!("Could not allocate the bounded qualification request"))?;
        serialize_qualification(self, &input, &questions, true, &mut body)
            .map_err(|_| anyhow!("Could not serialize the bounded image qualification request"))?;
        self.execution.remaining()?;
        let request = self.bind_prepared_body(body.bytes, questions);
        Ok(PreparedImageQualification { request, input })
    }

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

#[derive(Serialize)]
struct QualificationBody<'a> {
    model: &'a str,
    state: QualificationState<'a>,
    questions: &'a Value,
    provider: NoFallbacks,
}

#[derive(Serialize)]
struct NoFallbacks {
    allow_fallbacks: bool,
}

struct QualificationState<'a> {
    input: &'a DecisionInput,
    include_images: bool,
}

impl Serialize for QualificationState<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let images = if self.include_images {
            self.input.images.len()
        } else {
            0
        };
        let mut state = serializer.serialize_seq(Some(self.input.text.len() + images))?;
        for item in &self.input.text {
            state.serialize_element(&item.text)?;
        }
        if self.include_images {
            for image in &self.input.images {
                state.serialize_element(&QualificationImage(image))?;
            }
        }
        state.end()
    }
}

struct QualificationImage<'a>(&'a ImageEvidence);

impl Serialize for QualificationImage<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct ImagePart<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            image_url: ImageUrl<'a>,
        }
        #[derive(Serialize)]
        struct ImageUrl<'a> {
            url: &'a str,
        }
        let image = self.0;
        let prefix = match image.media_type {
            "image/png" => "data:image/png;base64,",
            "image/jpeg" => "data:image/jpeg;base64,",
            _ => {
                return Err(serde::ser::Error::custom(
                    "Unsupported qualification image media",
                ));
            }
        };
        let mut url = String::new();
        url.try_reserve_exact(prefix.len() + image.base64_bytes())
            .map_err(|_| serde::ser::Error::custom("Could not allocate bounded image encoding"))?;
        url.push_str(prefix);
        STANDARD.encode_string(&image.bytes, &mut url);
        ImagePart {
            kind: "image_url",
            image_url: ImageUrl { url: &url },
        }
        .serialize(serializer)
    }
}

fn serialize_qualification(
    client: &JevClient,
    input: &DecisionInput,
    questions: &Value,
    include_images: bool,
    writer: &mut BoundedWriter,
) -> serde_json::Result<()> {
    serde_json::to_writer(
        writer,
        &QualificationBody {
            model: &client.model,
            state: QualificationState {
                input,
                include_images,
            },
            questions,
            provider: NoFallbacks {
                allow_fallbacks: false,
            },
        },
    )
}

/// Count or retain serialized bytes, rejecting before a write can cross the cap.
struct BoundedWriter {
    bytes: Vec<u8>,
    len: usize,
    limit: usize,
    retain: bool,
}

impl BoundedWriter {
    fn counting(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            len: 0,
            limit,
            retain: false,
        }
    }

    fn buffered(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            len: 0,
            limit,
            retain: true,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .len
            .checked_add(bytes.len())
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Bounded Decisions serialization exceeded its byte limit",
                )
            })?;
        if self.retain {
            // Request bounded growth; allocator physical-block guarantees are separate.
            self.bytes.try_reserve_exact(bytes.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "Could not allocate bounded Decisions request",
                )
            })?;
            self.bytes.extend_from_slice(bytes);
        }
        self.len = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "image_qualification_tests.rs"]
mod image_qualification_tests;
