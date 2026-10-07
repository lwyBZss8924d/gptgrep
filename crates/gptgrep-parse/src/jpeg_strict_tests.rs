use super::*;

fn segment(output: &mut Vec<u8>, marker: u8, payload: &[u8]) {
    output.extend_from_slice(&[0xff, marker]);
    output.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    output.extend_from_slice(payload);
}

pub(crate) fn fixture(width: u16, height: u16, dc_values: &[i32]) -> Vec<u8> {
    assert!(matches!(dc_values.len(), 1 | 3));
    let mut bytes = vec![0xff, 0xd8];
    let mut quant = vec![0];
    quant.extend_from_slice(&[1; 64]);
    segment(&mut bytes, 0xdb, &quant);
    let mut frame = vec![8];
    frame.extend_from_slice(&height.to_be_bytes());
    frame.extend_from_slice(&width.to_be_bytes());
    frame.push(dc_values.len() as u8);
    let ids: &[u8] = if dc_values.len() == 1 { &[1] } else { b"RGB" };
    for id in ids {
        frame.extend_from_slice(&[*id, 0x11, 0]);
    }
    segment(&mut bytes, 0xc0, &frame);
    let mut dc = vec![0; 17];
    dc[4] = 12;
    dc.extend(0..12);
    let mut ac = vec![0; 17];
    ac[0] = 0x10;
    ac[1] = 1;
    ac.push(0);
    dc.extend(ac);
    segment(&mut bytes, 0xc4, &dc);
    let mut scan = vec![dc_values.len() as u8];
    for id in ids {
        scan.extend_from_slice(&[*id, 0]);
    }
    scan.extend_from_slice(&[0, 63, 0]);
    segment(&mut bytes, 0xda, &scan);
    let mut bits = Vec::new();
    for mcu in 0..u32::from(width).div_ceil(8) * u32::from(height).div_ceil(8) {
        for value in dc_values {
            let difference = if mcu == 0 { *value } else { 0 };
            let category = 32 - difference.unsigned_abs().leading_zeros();
            for bit in (0..4).rev() {
                bits.push(((category >> bit) & 1) as u8);
            }
            let magnitude = if difference >= 0 {
                difference
            } else {
                difference + (1 << category) - 1
            };
            for bit in (0..category).rev() {
                bits.push(((magnitude >> bit) & 1) as u8);
            }
            bits.push(0); // AC EOB.
        }
    }
    while !bits.len().is_multiple_of(8) {
        bits.push(1);
    }
    for chunk in bits.chunks_exact(8) {
        let byte = chunk.iter().fold(0, |byte, bit| (byte << 1) | bit);
        bytes.push(byte);
        if byte == 0xff {
            bytes.push(0);
        }
    }
    bytes.extend_from_slice(&[0xff, 0xd9]);
    bytes
}

#[test]
fn actual_gray_and_rgb_entropy_decode_to_known_constant_samples() {
    for (values, expected) in [
        (&[0][..], &[128][..]),
        (&[-1024, 0, 1016][..], &[0, 128, 255][..]),
    ] {
        let bytes = fixture(13, 9, values);
        let h = header(&bytes).unwrap();
        h.validate_entropy(&bytes).unwrap();
        let mut output = vec![0; 13 * 9 * expected.len()];
        h.decode_into(&bytes, &mut output).unwrap();
        assert!(
            output
                .chunks_exact(expected.len())
                .all(|pixel| pixel == expected)
        );
        assert_eq!(h.scratch_bytes(), 16 * values.len() as u64 * 16);
    }
}

#[test]
fn incomplete_entropy_at_eoi_is_accepted_unguarded_but_refused_by_owned_gate() {
    let bytes = fixture(8, 8, &[0]);
    let h = header(&bytes).unwrap();
    let mut incomplete = bytes[..h.entropy_offset].to_vec();
    incomplete.extend_from_slice(&[0xff, 0xd9]);
    let h = header(&incomplete).unwrap();
    let mut recovered = vec![0; 64];
    // Newly authored ordinary regression. This is distinct from the historical
    // source-only I1 finding, and proves why strict=true alone is insufficient.
    h.decode_into(&incomplete, &mut recovered).unwrap();
    assert_eq!(recovered, vec![128; 64]);
    let mut permissive = JpegDecoder::new_with_options(
        ZCursor::new(incomplete.as_slice()),
        DecoderOptions::default()
            .set_strict_mode(false)
            .jpeg_set_out_colorspace(ColorSpace::Luma),
    );
    permissive.decode_into(&mut recovered).unwrap();
    // The original preparation path used this exact pinned image wrapper,
    // whose final zune decoder selects strict=false.
    let wrapper = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&incomplete)).unwrap();
    image::ImageDecoder::read_image(wrapper, &mut recovered).unwrap();
    assert_eq!(recovered, vec![128; 64]);
    assert!(
        h.validate_entropy(&incomplete)
            .unwrap_err()
            .to_string()
            .contains("premature entropy marker")
    );
}

#[test]
fn invalid_huffman_categories_padding_and_terminal_content_are_refused() {
    let bytes = fixture(8, 8, &[0]);
    let dht = bytes
        .windows(2)
        .position(|pair| pair == [0xff, 0xc4])
        .unwrap();
    let mut bad = bytes.clone();
    bad[dht + 5] = 2;
    assert!(header(&bad).unwrap_err_string().contains("Huffman"));
    let mut bad = bytes.clone();
    bad[dht + 4 + 17] = 12;
    assert!(header(&bad).unwrap_err_string().contains("DC category"));
    let mut bad = bytes.clone();
    bad[dht + 4 + 17 + 12 + 17] = 0x10;
    assert!(header(&bad).unwrap_err_string().contains("AC category"));
    let h = header(&bytes).unwrap();
    let mut bad = bytes.clone();
    bad[h.entropy_offset] &= !1;
    assert!(
        h.validate_entropy(&bad)
            .unwrap_err()
            .to_string()
            .contains("padding")
    );
    for bad in [
        bytes[..bytes.len() - 2].to_vec(),
        {
            let mut b = bytes.clone();
            b.push(0);
            b
        },
        {
            let mut b = bytes.clone();
            let n = b.len();
            b[n - 1] = 0xd8;
            b
        },
    ] {
        assert!(
            h.validate_entropy(&bad)
                .unwrap_err()
                .to_string()
                .contains("EOI")
        );
    }
}

trait HeaderError {
    fn unwrap_err_string(self) -> String;
}
impl HeaderError for Result<Header> {
    fn unwrap_err_string(self) -> String {
        match self {
            Ok(_) => panic!("expected refused JPEG"),
            Err(error) => error.to_string(),
        }
    }
}

#[test]
fn coefficient_overflow_cannot_be_hidden_by_decoder_recovery() {
    let mut bytes = fixture(8, 8, &[0]);
    let dht = bytes
        .windows(2)
        .position(|pair| pair == [0xff, 0xc4])
        .unwrap();
    bytes[dht + 4 + 17 + 12 + 17] = 0xf0;
    let h = header(&bytes).unwrap();
    bytes.truncate(h.entropy_offset);
    bytes.extend_from_slice(&[0, 0xff, 0xd9]); // DC category0 (0000), four zero runs (0000).
    assert!(
        h.validate_entropy(&bytes)
            .unwrap_err()
            .to_string()
            .contains("zero run overflow")
    );
    let mut bytes = fixture(8, 8, &[0]);
    bytes[dht + 4 + 17 + 12 + 17] = 0xf1;
    let h = header(&bytes).unwrap();
    bytes.truncate(h.entropy_offset);
    bytes.extend_from_slice(&[0x05, 0x5f, 0xff, 0xd9]);
    assert!(
        h.validate_entropy(&bytes)
            .unwrap_err()
            .to_string()
            .contains("AC run overflow")
    );
    let mut bytes = fixture(8, 8, &[-1025]);
    assert!(
        header(&bytes)
            .unwrap()
            .validate_entropy(&bytes)
            .unwrap_err()
            .to_string()
            .contains("DC coefficient")
    );
    // Magnitude truncation is independently rejected rather than padded.
    let h = header(&bytes).unwrap();
    bytes.truncate(h.entropy_offset + 1);
    bytes.extend_from_slice(&[0xff, 0xd9]);
    assert!(h.validate_entropy(&bytes).is_err());
}

#[test]
fn unsupported_allocating_or_transforming_modes_fail_before_decoder() {
    let bytes = fixture(8, 8, &[0]);
    let sof = bytes
        .windows(2)
        .position(|pair| pair == [0xff, 0xc0])
        .unwrap();
    for (offset, value) in [
        (sof + 1, 0xc2),
        (sof + 4, 12),
        (sof + 9, 4),
        (sof + 11, 0x21),
    ] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        assert!(
            header(&bad)
                .unwrap_err_string()
                .contains("capability_unavailable")
        );
    }
    for marker in [0xe1, 0xe2, 0xed, 0xee, 0xdd, 0xd0] {
        let mut bad = bytes.clone();
        bad.splice(2..2, [0xff, marker, 0, 2]);
        assert!(
            header(&bad)
                .unwrap_err_string()
                .contains("capability_unavailable")
        );
    }
}
