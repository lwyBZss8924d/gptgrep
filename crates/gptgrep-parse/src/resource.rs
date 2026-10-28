//! Parser admission limits with configurable image and retained-staging capacity.

use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceBudget {
    pub image_file_bytes: u64,
    pub image_base64_bytes: u64,
    pub image_pixels: u64,
    pub image_pixel_buffer_bytes: u64,
    pub images_per_page: usize,
    pub image_pixels_per_page: u64,
    pub image_file_bytes_per_page: u64,
    pub page_layout_bytes: u64,
    pub document_metadata_bytes: u64,
    pub native_layout_input_bytes: u64,
    pub ocr_result_bytes: u64,
    pub ocr_regions: usize,
    pub ocr_region_text_bytes: u64,
    pub owned_working_bytes: u64,
    pub document_staging_bytes: u64,
    pub run_staging_bytes: u64,
    #[serde(skip)]
    staging_used: Arc<AtomicU64>,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            image_file_bytes: 1_048_576,
            image_base64_bytes: 1_398_104,
            image_pixels: 8_000_000,
            image_pixel_buffer_bytes: 33_554_432,
            images_per_page: 4,
            image_pixels_per_page: 24_000_000,
            image_file_bytes_per_page: 4_194_304,
            page_layout_bytes: 4_194_304,
            document_metadata_bytes: 67_108_864,
            native_layout_input_bytes: 65_536,
            ocr_result_bytes: 262_144,
            ocr_regions: 1024,
            ocr_region_text_bytes: 16_384,
            owned_working_bytes: 268_435_456,
            document_staging_bytes: 536_870_912,
            run_staging_bytes: 1_073_741_824,
            staging_used: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ResourceBudget {
    /// Verify shared staging custody and limits before transferring a product
    /// to another component of the same invocation.
    pub fn ensure_same_invocation_within(&self, admitted: &Self) -> Result<()> {
        self.validate()?;
        ensure!(
            Arc::ptr_eq(&self.staging_used, &admitted.staging_used),
            "resource_limit: resource budget belongs to a different invocation"
        );
        let selected = serde_json::to_value(self)?;
        let original = serde_json::to_value(admitted)?;
        for (field, value) in selected.as_object().expect("budget object") {
            ensure!(
                value.as_u64().expect("unsigned budget")
                    <= original[field].as_u64().expect("admitted budget"),
                "resource_limit: {field} exceeds the original admission"
            );
        }
        Ok(())
    }
    /// Clones share an invocation's retained staging count. Failed candidates
    /// retain their reservation because their bytes are retained for inspection.
    pub fn staging_used(&self) -> u64 {
        self.staging_used.load(Ordering::SeqCst)
    }

    /// Reserve bytes before retaining or copying staging artifacts. Clones
    /// share this counter for one invocation; failed reservations are atomic,
    /// and retained failed candidates are deliberately not refunded.
    pub fn reserve_retained_staging(&self, bytes: u64) -> Result<()> {
        self.staging_used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= self.run_staging_bytes)
            })
            .map_err(|_| anyhow::anyhow!("resource_limit: shared run staging admission failed"))?;
        Ok(())
    }
    /// Rehydrate a portable tighter budget onto the server's retained staging
    /// counter. Serialized limits are preserved; only invocation custody changes.
    pub fn adopt_shared_staging(&mut self, admitted: &Self) -> Result<()> {
        self.validate()?;
        admitted.validate()?;
        let selected = serde_json::to_value(&*self)?;
        let ceiling = serde_json::to_value(admitted)?;
        for (field, value) in selected.as_object().expect("budget object") {
            ensure!(
                value.as_u64().expect("unsigned budget")
                    <= ceiling[field].as_u64().expect("budget"),
                "resource_limit: selected budget exceeds server admission"
            );
        }
        self.staging_used = admitted.staging_used.clone();
        Ok(())
    }
    pub(crate) fn reserve_staging(&self, bytes: u64) -> Result<()> {
        self.reserve_retained_staging(bytes)
    }
    pub fn validate(&self) -> Result<()> {
        let selected = serde_json::to_value(self)?;
        let frozen = serde_json::to_value(Self::default())?;
        for (field, value) in selected.as_object().expect("budget object") {
            let value = value.as_u64().expect("unsigned budget");
            let configurable = matches!(
                field.as_str(),
                "image_file_bytes"
                    | "image_base64_bytes"
                    | "image_file_bytes_per_page"
                    | "document_staging_bytes"
                    | "run_staging_bytes"
                    | "owned_working_bytes"
            );
            ensure!(
                value > 0
                    && (configurable || value <= frozen[field].as_u64().expect("frozen budget")),
                "resource_limit: {field} must be positive and within its admission"
            );
        }
        ensure!(
            self.image_base64_bytes >= base64_bytes(self.image_file_bytes)?,
            "resource_limit: base64 budget does not cover the image file budget"
        );
        ensure!(
            self.image_file_bytes <= self.image_file_bytes_per_page,
            "resource_limit: image and page byte budgets disagree"
        );
        base64_bytes(self.image_file_bytes_per_page)?;
        let encoded_working = self
            .image_file_bytes
            .checked_add(self.image_base64_bytes)
            .ok_or_else(|| anyhow::anyhow!("resource_limit: encoded image working overflow"))?;
        usize::try_from(encoded_working)?;
        usize::try_from(self.document_staging_bytes)?;
        usize::try_from(self.run_staging_bytes)?;
        usize::try_from(self.owned_working_bytes)?;
        // Capacities need not all fit at once: text-only work can select a
        // smaller working/staging envelope. Actual render, decode, encoding and
        // retention paths reserve their checked byte totals before allocation.
        Ok(())
    }

    /// The JSON wrapper is bounded separately from the base64 image. Native
    /// call IDs are at most 256 bytes; 64 KiB also covers the finite MIME/detail
    /// fields. This is framing allowance, not an encoded-image ceiling.
    pub fn native_image_limits(&self) -> Result<NativeImageLimits> {
        self.validate()?;
        let framing = 65_536u64;
        let images = u64::try_from(self.images_per_page)?;
        let image_wire_bytes = self
            .image_base64_bytes
            .checked_add(framing)
            .ok_or_else(|| anyhow::anyhow!("resource_limit: native image wire overflow"))?;
        // Each separately encoded image may contribute one extra base64 group.
        let image_transport_bytes = base64_bytes(self.image_file_bytes_per_page)?
            .checked_add(
                images.checked_mul(framing + 4).ok_or_else(|| {
                    anyhow::anyhow!("resource_limit: native image framing overflow")
                })?,
            )
            .ok_or_else(|| anyhow::anyhow!("resource_limit: native image transport overflow"))?;
        let limits = NativeImageLimits {
            image_file_bytes: self.image_file_bytes,
            image_base64_bytes: self.image_base64_bytes,
            image_wire_bytes,
            image_transport_bytes,
        };
        limits.validate()?;
        ensure!(
            limits.transport_bytes()? <= self.owned_working_bytes,
            "resource_limit: native image transport exceeds owned working budget"
        );
        Ok(limits)
    }

    /// Check geometry before allocating a pixel buffer or decoding an image.
    pub fn admit_image(&self, width: u32, height: u32, encoded_bytes: u64) -> Result<u64> {
        self.validate()?;
        ensure!(width > 0 && height > 0, "invalid image dimensions");
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| {
                anyhow::anyhow!("resource_limit: image pixel multiplication overflow")
            })?;
        let rgba = pixels.checked_mul(4).ok_or_else(|| {
            anyhow::anyhow!("resource_limit: decoded image multiplication overflow")
        })?;
        let base64 = base64_bytes(encoded_bytes)?;
        ensure!(
            pixels <= self.image_pixels
                && rgba <= self.image_pixel_buffer_bytes
                && encoded_bytes <= self.image_file_bytes
                && base64 <= self.image_base64_bytes,
            "resource_limit: image exceeds admitted pixel/file/base64/decode budget"
        );
        Ok(pixels)
    }
}

/// Limits bound into the native runtime profile, manifest, and ready receipt.
/// RPC frames have a separate unchanged admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeImageLimits {
    pub image_file_bytes: u64,
    pub image_base64_bytes: u64,
    pub image_wire_bytes: u64,
    pub image_transport_bytes: u64,
}
impl NativeImageLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.image_file_bytes > 0
                && self.image_base64_bytes >= base64_bytes(self.image_file_bytes)?
                && self.image_wire_bytes > self.image_base64_bytes
                && self.image_transport_bytes >= self.image_wire_bytes,
            "resource_limit: native image limits disagree"
        );
        usize::try_from(self.image_wire_bytes)?;
        usize::try_from(
            self.image_file_bytes
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("resource_limit: native image read overflow"))?,
        )?;
        self.transport_bytes()?;
        Ok(())
    }
    pub fn transport_bytes(&self) -> Result<u64> {
        self.image_transport_bytes
            .checked_add(8 * 1024 * 1024)
            .ok_or_else(|| anyhow::anyhow!("resource_limit: native transport overflow"))
    }
}

fn base64_bytes(bytes: u64) -> Result<u64> {
    bytes
        .checked_add(2)
        .and_then(|n| n.checked_div(3))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("resource_limit: base64 length overflow"))
}

#[cfg(test)]
#[path = "resource_tests.rs"]
mod tests;

pub(crate) struct LimitedWriter<W> {
    pub(crate) inner: W,
    pub(crate) limit: u64,
    pub(crate) written: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("resource_limit: serialized length overflow"))?;
        if next > self.limit {
            return Err(io::Error::other(
                "resource_limit: bounded writer admission failed",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn encoded_size(value: &impl Serialize, limit: u64) -> Result<u64> {
    let mut writer = LimitedWriter {
        inner: io::sink(),
        limit,
        written: 0,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.written)
}
