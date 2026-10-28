//! Owned admission for a deliberately narrow JPEG subset. Entropy is checked
//! using only actual source bits before the pinned decoder can synthesize any.

use anyhow::{Result, bail, ensure};
use image::ColorType;
use zune_jpeg::{
    JpegDecoder,
    zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions},
};

#[derive(Clone, Copy, Default)]
struct Component {
    id: u8,
    quant: u8,
    dc: usize,
    ac: usize,
}

#[derive(Clone)]
struct Huffman {
    minimum: [u32; 17],
    counts: [u16; 17],
    first: [u16; 17],
    symbols: [u8; 256],
}

impl Default for Huffman {
    fn default() -> Self {
        Self {
            minimum: [0; 17],
            counts: [0; 17],
            first: [0; 17],
            symbols: [0; 256],
        }
    }
}

impl Huffman {
    fn parse(lengths: &[u8], values: &[u8], dc: bool) -> Result<Self> {
        ensure!(
            lengths.len() == 16 && !values.is_empty() && values.len() <= 256,
            "invalid JPEG Huffman symbol count"
        );
        let mut table = Self::default();
        let mut code = 0u32;
        let mut first = 0u16;
        for length in 1..=16 {
            let count = u16::from(lengths[length - 1]);
            ensure!(
                code + u32::from(count) < (1u32 << length),
                "invalid JPEG Huffman tree or all-one code"
            );
            table.minimum[length] = code;
            table.counts[length] = count;
            table.first[length] = first;
            first += count;
            code = (code + u32::from(count)) << 1;
        }
        ensure!(
            usize::from(first) == values.len(),
            "invalid JPEG Huffman length"
        );
        let mut seen = [false; 256];
        for (index, value) in values.iter().copied().enumerate() {
            ensure!(
                !seen[usize::from(value)],
                "unsupported duplicate JPEG Huffman symbol"
            );
            seen[usize::from(value)] = true;
            if dc {
                ensure!(value <= 11, "invalid baseline JPEG DC category");
            } else {
                let size = value & 15;
                ensure!(
                    size <= 10 && (size != 0 || value == 0 || value == 0xf0),
                    "invalid baseline JPEG AC category"
                );
            }
            table.symbols[index] = value;
        }
        Ok(table)
    }

    fn symbol(&self, bits: &mut Entropy<'_>) -> Result<u8> {
        let mut code = 0u32;
        for length in 1..=16 {
            code = (code << 1) | u32::from(bits.bit()?);
            let minimum = self.minimum[length];
            if code >= minimum && code - minimum < u32::from(self.counts[length]) {
                let index = usize::from(self.first[length]) + usize::try_from(code - minimum)?;
                return Ok(self.symbols[index]);
            }
        }
        bail!("invalid JPEG entropy Huffman code")
    }
}

pub(super) struct Header {
    pub width: u32,
    pub height: u32,
    count: usize,
    components: [Component; 3],
    tables: [Option<Huffman>; 8],
    entropy_offset: usize,
    rgb_input: bool,
}

impl Header {
    pub fn color(&self) -> ColorType {
        if self.count == 1 {
            ColorType::L8
        } else {
            ColorType::Rgb8
        }
    }

    /// No decoder allocation is allowed until the caller admits this bound.
    /// The subset has no upsampling or whole-frame coefficient storage.
    pub fn scratch_bytes(&self) -> u64 {
        16 * self.count as u64 * u64::from(self.width).div_ceil(8) * 8
    }

    pub fn validate_entropy(&self, bytes: &[u8]) -> Result<()> {
        let mut bits = Entropy {
            bytes,
            offset: self.entropy_offset,
            byte: 0,
            left: 0,
        };
        let mcus = u64::from(self.width).div_ceil(8) * u64::from(self.height).div_ceil(8);
        let mut predictions = [0i32; 3];
        for _ in 0..mcus {
            for (index, component) in self.components[..self.count].iter().enumerate() {
                let dc = self.tables[component.dc]
                    .as_ref()
                    .expect("header checked table");
                let category = dc.symbol(&mut bits)?;
                let value = bits.magnitude(category)?;
                predictions[index] = predictions[index]
                    .checked_add(value)
                    .ok_or_else(|| anyhow::anyhow!("invalid JPEG DC prediction overflow"))?;
                ensure!(
                    (-1024..=1023).contains(&predictions[index]),
                    "invalid baseline JPEG DC coefficient"
                );
                let ac = self.tables[4 + component.ac]
                    .as_ref()
                    .expect("header checked table");
                let mut position = 1usize;
                while position < 64 {
                    let symbol = ac.symbol(&mut bits)?;
                    if symbol == 0 {
                        break;
                    }
                    if symbol == 0xf0 {
                        ensure!(position + 16 <= 64, "invalid JPEG zero run overflow");
                        position += 16;
                    } else {
                        position += usize::from(symbol >> 4);
                        ensure!(position < 64, "invalid JPEG AC run overflow");
                        bits.magnitude(symbol & 15)?;
                        position += 1;
                    }
                }
            }
        }
        bits.finish()
    }

    pub fn decode_into(&self, bytes: &[u8], output: &mut [u8]) -> Result<()> {
        let output_space = if self.count == 1 {
            ColorSpace::Luma
        } else {
            ColorSpace::RGB
        };
        let expected_input = if self.count == 1 {
            ColorSpace::Luma
        } else if self.rgb_input {
            ColorSpace::RGB
        } else {
            ColorSpace::YCbCr
        };
        let options = DecoderOptions::default()
            .set_strict_mode(true)
            .set_use_unsafe(false)
            .set_max_width(self.width as usize)
            .set_max_height(self.height as usize)
            .jpeg_set_max_scans(1)
            .jpeg_set_out_colorspace(output_space);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
        decoder.decode_headers()?;
        ensure!(
            decoder.dimensions() == Some((self.width as usize, self.height as usize))
                && decoder.input_colorspace() == Some(expected_input)
                && decoder.output_colorspace() == Some(output_space)
                && decoder.output_buffer_size() == Some(output.len()),
            "strict JPEG decoder/header mismatch"
        );
        decoder.decode_into(output)?;
        Ok(())
    }
}

struct Entropy<'a> {
    bytes: &'a [u8],
    offset: usize,
    byte: u8,
    left: u8,
}

impl Entropy<'_> {
    fn bit(&mut self) -> Result<u8> {
        if self.left == 0 {
            let byte = *self
                .bytes
                .get(self.offset)
                .ok_or_else(|| anyhow::anyhow!("invalid JPEG incomplete entropy"))?;
            self.offset += 1;
            if byte == 0xff {
                let next = self
                    .bytes
                    .get(self.offset)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("invalid JPEG incomplete byte stuffing"))?;
                ensure!(
                    !matches!(next, 0xd0..=0xd7 | 0xda),
                    "capability_unavailable: JPEG multiple scans or restarts"
                );
                ensure!(next == 0, "invalid JPEG premature entropy marker");
                self.offset += 1;
            }
            self.byte = byte;
            self.left = 8;
        }
        self.left -= 1;
        Ok((self.byte >> self.left) & 1)
    }

    fn magnitude(&mut self, size: u8) -> Result<i32> {
        if size == 0 {
            return Ok(0);
        }
        let mut value = 0i32;
        for _ in 0..size {
            value = (value << 1) | i32::from(self.bit()?);
        }
        Ok(if value < (1 << (size - 1)) {
            value + 1 - (1 << size)
        } else {
            value
        })
    }

    fn finish(self) -> Result<()> {
        let mask = (1u16 << self.left) - 1;
        ensure!(
            u16::from(self.byte) & mask == mask,
            "invalid JPEG entropy padding"
        );
        ensure!(
            !self
                .bytes
                .get(self.offset..)
                .is_some_and(|remaining| remaining.starts_with(&[0xff, 0xda])
                    || remaining.starts_with(&[0xff, 0xdd])
                    || matches!(remaining.get(1), Some(0xd0..=0xd7))),
            "capability_unavailable: JPEG multiple scans or restarts"
        );
        ensure!(
            self.bytes.get(self.offset..) == Some(&[0xff, 0xd9][..]),
            "invalid JPEG terminal EOI or trailing entropy/content"
        );
        Ok(())
    }
}

/// Parse only fixed-size tables/scalars. APP metadata, nonbaseline sampling and
/// scan layouts which would change decoder allocations fail before construction.
pub(super) fn header(bytes: &[u8]) -> Result<Header> {
    ensure!(bytes.starts_with(&[0xff, 0xd8]), "invalid JPEG signature");
    let mut offset = 2usize;
    let mut header = Header {
        width: 0,
        height: 0,
        count: 0,
        components: [Component::default(); 3],
        tables: core::array::from_fn(|_| None),
        entropy_offset: 0,
        rgb_input: false,
    };
    let mut quant = 0u8;
    let mut jfif = false;
    loop {
        ensure!(
            bytes.get(offset) == Some(&0xff),
            "invalid JPEG marker prefix"
        );
        let marker = *bytes
            .get(offset + 1)
            .ok_or_else(|| anyhow::anyhow!("invalid JPEG truncated marker"))?;
        offset += 2;
        ensure!(
            matches!(marker, 0xc0 | 0xc4 | 0xdb | 0xda | 0xe0 | 0xfe),
            "capability_unavailable: JPEG requires baseline8bit gray/RGB1:1 single-scan without restarts or metadata transforms"
        );
        let length = bytes
            .get(offset..offset + 2)
            .ok_or_else(|| anyhow::anyhow!("invalid JPEG truncated segment length"))?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        ensure!(length >= 2, "invalid JPEG segment length");
        let end = offset
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("invalid JPEG length overflow"))?;
        let payload = bytes
            .get(offset + 2..end)
            .ok_or_else(|| anyhow::anyhow!("invalid JPEG truncated segment"))?;
        offset = end;
        match marker {
            0xe0 => {
                ensure!(
                    !jfif
                        && payload.len() == 14
                        && payload.starts_with(b"JFIF\0")
                        && payload[5] == 1
                        && payload[6] <= 2
                        && payload[7] <= 2
                        && payload[8..10] != [0, 0]
                        && payload[10..12] != [0, 0]
                        && payload[12..14] == [0, 0],
                    "capability_unavailable: JPEG requires bounded JFIF without thumbnails"
                );
                jfif = true;
            }
            0xfe => {} // A bounded non-transforming comment; zune skips it.
            0xdb => {
                ensure!(
                    !payload.is_empty() && payload.len().is_multiple_of(65),
                    "invalid JPEG quantization table"
                );
                for table in payload.chunks_exact(65) {
                    let index = table[0];
                    ensure!(
                        index <= 3 && quant & (1 << index) == 0 && !table[1..].contains(&0),
                        "invalid or unsupported JPEG quantization definition"
                    );
                    quant |= 1 << index;
                }
            }
            0xc4 => {
                let mut cursor = 0usize;
                ensure!(!payload.is_empty(), "invalid JPEG empty Huffman segment");
                while cursor < payload.len() {
                    let prefix = payload.get(cursor..cursor + 17).ok_or_else(|| {
                        anyhow::anyhow!("invalid JPEG truncated Huffman definition")
                    })?;
                    let kind = prefix[0] >> 4;
                    let index = usize::from(prefix[0] & 15);
                    ensure!(kind <= 1 && index <= 3, "invalid JPEG Huffman selector");
                    let count: usize = prefix[1..].iter().map(|x| usize::from(*x)).sum();
                    let values = payload
                        .get(cursor + 17..cursor + 17 + count)
                        .ok_or_else(|| anyhow::anyhow!("invalid JPEG truncated Huffman symbols"))?;
                    let slot = index + usize::from(kind) * 4;
                    ensure!(
                        header.tables[slot].is_none(),
                        "capability_unavailable: JPEG Huffman redefinition"
                    );
                    header.tables[slot] = Some(Huffman::parse(&prefix[1..], values, kind == 0)?);
                    cursor += 17 + count;
                }
            }
            0xc0 => {
                ensure!(
                    header.count == 0 && payload.len() >= 6,
                    "invalid JPEG duplicate/truncated frame"
                );
                ensure!(
                    payload[0] == 8 && matches!(payload[5], 1 | 3),
                    "capability_unavailable: JPEG requires8bit gray/RGB only"
                );
                header.height = u32::from(u16::from_be_bytes([payload[1], payload[2]]));
                header.width = u32::from(u16::from_be_bytes([payload[3], payload[4]]));
                ensure!(
                    header.width > 0 && header.height > 0,
                    "invalid JPEG zero frame dimension"
                );
                header.count = usize::from(payload[5]);
                ensure!(
                    payload.len() == 6 + 3 * header.count,
                    "invalid JPEG frame length"
                );
                for (component, fields) in header.components[..header.count]
                    .iter_mut()
                    .zip(payload[6..].chunks_exact(3))
                {
                    ensure!(
                        fields[1] == 0x11 && fields[2] <= 3,
                        "capability_unavailable: JPEG requires1:1 sampling and bounded quantization selectors"
                    );
                    component.id = fields[0];
                    component.quant = fields[2];
                }
                if header.count == 3 {
                    let ids = header.components.map(|component| component.id);
                    ensure!(
                        ids == [1, 2, 3] || ids == [b'R', b'G', b'B'],
                        "capability_unavailable: JPEG requires explicit RGB or canonical YCbCr components"
                    );
                    header.rgb_input = ids == [b'R', b'G', b'B'];
                }
            }
            0xda => {
                ensure!(
                    header.count > 0
                        && payload.len() == 4 + 2 * header.count
                        && usize::from(payload[0]) == header.count,
                    "capability_unavailable: JPEG requires one scan with all frame components"
                );
                let scan_end = 1 + 2 * header.count;
                ensure!(
                    payload[scan_end..] == [0, 63, 0],
                    "capability_unavailable: JPEG requires baseline scan parameters"
                );
                for (component, fields) in header.components[..header.count]
                    .iter_mut()
                    .zip(payload[1..scan_end].chunks_exact(2))
                {
                    ensure!(
                        fields[0] == component.id,
                        "invalid or unsupported JPEG scan component order"
                    );
                    component.dc = usize::from(fields[1] >> 4);
                    component.ac = usize::from(fields[1] & 15);
                    ensure!(
                        component.dc < 4
                            && component.ac < 4
                            && header.tables[component.dc].is_some()
                            && header.tables[4 + component.ac].is_some()
                            && quant & (1 << component.quant) != 0,
                        "invalid JPEG missing referenced table"
                    );
                }
                header.entropy_offset = offset;
                return Ok(header);
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
#[path = "jpeg_strict_tests.rs"]
pub(crate) mod tests;
