use super::*;
use serde_json::json;

fn summary() -> PreparedSummary {
    PreparedSummary {
        source_sha256: "1".repeat(64),
        prepared_sha256: "2".repeat(64),
        source_bytes: 10,
        prepared_bytes: 10,
        format: "pdf".into(),
        parser_identity: crate::RICH_PARSER.into(),
        preparation_profile_sha256: "3".repeat(64),
        conversion: FeatureState::Absent,
        image_geometry: FeatureState::Absent,
    }
}

fn observed(c: char, x: f32, y: f32, font: &[u8], flags: i32) -> NativeCharacterObservation {
    NativeCharacterObservation {
        unicode: c as u32,
        char_code: c as u32,
        has_unicode_map_error: false,
        generated: false,
        bounds: Some(NativeBounds {
            x,
            y,
            width: 5.0,
            height: 10.0,
        }),
        font_size: 12.0,
        font_weight: 400,
        font: font_state(Some((font.to_vec(), flags))),
    }
}

fn geometryless(c: char, generated: bool) -> NativeCharacterObservation {
    let mut value = observed(c, 0.0, 0.0, b"Helvetica", 0);
    value.bounds = None;
    value.font_size = 0.0;
    value.generated = generated;
    value
}

// Both the real FFI observer in extract_sync and these scalar authored observers
// pass through this production collector. No already-filtered native DTO is
// supplied, and no special test-only retention implementation exists.
fn source(observations: Vec<NativeCharacterObservation>) -> Vec<NativeCharacter> {
    let count = observations.len() as u32;
    let mut values = observations.into_iter();
    collect_characters(
        count,
        1500.0,
        300.0,
        &ResourceBudget::default(),
        NativeBoundsPolicy::LegacyViewport,
        |_| Ok(values.next().unwrap()),
    )
    .unwrap()
}

fn page(characters: &[NativeCharacter]) -> Page {
    Page {
        page_number: 1,
        page_label: None,
        page_width: 1500.0,
        page_height: 300.0,
        content_bounds: None,
        text_items: glyph_runs(characters),
        graphics: vec![],
        vector_graphics: None,
        struct_nodes: vec![],
        image_refs: vec![],
        annotations: None,
        form_fields: None,
        structure_tree: None,
    }
}

fn envelope(characters: Vec<NativeCharacter>) -> serde_json::Value {
    let (fonts, words) = capabilities(&characters);
    json!({"page":page(&characters),"native_text_metadata":metadata(&summary(),1,characters.len() as u32,characters),
        "capabilities":{"font_names":fonts,"word_segmentation":words,"native_stream":FeatureState::Present(STREAM_RECIPE)}})
}

fn validate(value: &serde_json::Value) -> Result<()> {
    validate_native_text_metadata(
        &serde_json::to_vec(value)?,
        &summary(),
        1,
        &ResourceBudget::default(),
    )
}

#[test]
fn v7_unclipped_characters_preserve_signed_outside_and_zero_area_without_changing_v6() {
    let make = || {
        let mut space = observed(' ', -7.0, 301.0, b"Helvetica", 32);
        space.bounds.as_mut().unwrap().width = 0.0;
        vec![
            observed('A', -20.0, -30.0, b"Helvetica", 32),
            observed('B', 1498.0, 299.0, b"Helvetica", 32),
            space,
            geometryless('\n', true),
        ]
    };
    let collect = |policy| {
        let mut observations = make().into_iter();
        collect_characters(4, 1500.0, 300.0, &ResourceBudget::default(), policy, |_| {
            Ok(observations.next().unwrap())
        })
    };
    assert!(collect(NativeBoundsPolicy::LegacyViewport).is_err());
    let characters = collect(NativeBoundsPolicy::UnclippedV7).unwrap();
    assert_eq!(
        characters.iter().map(character).collect::<String>(),
        "AB \n"
    );
    assert_eq!(
        characters.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    let NativeGeometry::Positioned(first) = &characters[0].geometry else {
        panic!("positioned")
    };
    assert_eq!(
        (first.x, first.y, first.width, first.height),
        (-20.0, -30.0, 5.0, 10.0)
    );
    assert!(matches!(
        characters[2].geometry,
        NativeGeometry::ZeroArea(_)
    ));
    assert!(characters[3].generated);
    let runs = unclipped_glyph_runs(&characters).unwrap();
    assert_eq!(runs[0].words[0].x, -20.0);
    assert_eq!(runs[1].words[0].y, 299.0);
    assert_eq!(runs[1].font_flags, Some(32));
    let mut value = envelope(characters);
    value["capabilities"]["native_character_geometry"] =
        json!(FeatureState::Present(UNCLIPPED_GEOMETRY));
    let bytes = serde_json::to_vec(&value).unwrap();
    validate_native_text_metadata_unclipped(&bytes, &summary(), 1, &ResourceBudget::default())
        .unwrap();
    assert!(validate(&value).is_err());
    for case in 0..4 {
        let mut forged = value.clone();
        match case {
            0 => forged["native_text_metadata"]["characters"][1]["ordinal"] = json!(0),
            1 => {
                forged["native_text_metadata"]["characters"][0]["font"]["value"]["flags"] = json!(0)
            }
            2 => forged["page"]["text_items"][0]["words"][0]["x"] = json!(0),
            _ => forged["capabilities"]["native_character_geometry"] = json!({"state":"absent"}),
        }
        assert!(
            validate_native_text_metadata_unclipped(
                &serde_json::to_vec(&forged).unwrap(),
                &summary(),
                1,
                &ResourceBudget::default()
            )
            .is_err(),
            "case {case}"
        );
    }
}

#[test]
fn v7_unclipped_geometry_rejects_nonfinite_negative_extent_endpoint_and_union_overflow() {
    for case in 0..5 {
        let mut value = observed('A', -20.0, -30.0, b"Helvetica", 0);
        let b = value.bounds.as_mut().unwrap();
        match case {
            0 => b.x = f32::NAN,
            1 => b.y = f32::INFINITY,
            2 => b.width = -1.0,
            3 => {
                b.x = f32::MAX;
                b.width = f32::MAX;
            }
            _ => {
                b.y = f32::MAX;
                b.height = f32::MAX;
            }
        }
        let mut value = Some(value);
        assert!(
            collect_characters(
                1,
                1500.0,
                300.0,
                &ResourceBudget::default(),
                NativeBoundsPolicy::UnclippedV7,
                |_| Ok(value.take().unwrap())
            )
            .is_err()
        );
    }
    // Each source box and endpoint is finite; their merged union is not.
    let mut values = [-2.0e38_f32, 0.0].into_iter().map(|x| {
        let mut o = observed('A', x, 0.0, b"Helvetica", 0);
        o.bounds.as_mut().unwrap().width = 3.0e38;
        o.bounds.as_mut().unwrap().height = 1.0e-10;
        o.font_size = f64::from(3.0e38_f32);
        o
    });
    let characters = collect_characters(
        2,
        1500.0,
        300.0,
        &ResourceBudget::default(),
        NativeBoundsPolicy::UnclippedV7,
        |_| Ok(values.next().unwrap()),
    )
    .unwrap();
    assert!(unclipped_glyph_runs(&characters).is_err());
}

#[test]
fn font_identity_and_flags_preserve_equal_numeric_style_boundaries() {
    let characters = source(vec![
        observed('a', 10.0, 20.0, b"Helvetica", 0),
        observed('b', 15.0, 20.0, b"Helvetica", 0),
        observed('c', 20.0, 20.0, b"Times-Roman", 0),
        observed('d', 25.0, 20.0, b"Times-Roman", 32),
    ]);
    let runs = glyph_runs(&characters);
    assert_eq!(
        runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
        ["ab", "c", "d"]
    );
    assert_eq!(runs[0].font_name.as_deref(), Some("Helvetica"));
    assert_eq!(runs[1].font_size, runs[0].font_size);
    assert_eq!(runs[1].font_weight, runs[0].font_weight);
    assert_eq!(runs[2].font_flags, Some(32));
    validate(&envelope(characters)).unwrap();
}

#[test]
fn glyph_words_preserve_unicode_actual_whitespace_and_exact_box_union() {
    let characters = source(vec![
        observed('é', 10.0, 20.0, b"Helvetica", 0),
        observed('a', 16.0, 21.0, b"Helvetica", 0),
        geometryless(' ', false),
        geometryless('\t', false),
        observed('β', 25.0, 20.0, b"Helvetica", 0),
    ]);
    let runs = glyph_runs(&characters);
    // TAB is explicitly a retained control boundary, not an invented space.
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].text, "éa ");
    assert_eq!(runs[1].text, "β");
    let words = runs.iter().flat_map(|r| &r.words).collect::<Vec<_>>();
    assert_eq!(
        words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
        ["éa", "β"]
    );
    assert_eq!(
        (words[0].x, words[0].y, words[0].width, words[0].height),
        (10.0, 20.0, 11.0, 11.0)
    );
    assert_eq!(
        (words[1].x, words[1].y, words[1].width, words[1].height),
        (25.0, 20.0, 5.0, 10.0)
    );
    assert_eq!(
        characters.iter().map(character).collect::<String>(),
        "éa \tβ"
    );
    validate(&envelope(characters)).unwrap();
}

#[test]
fn glyph_words_do_not_invent_spaces_across_font_line_or_large_gap_boundaries() {
    let characters = source(vec![
        observed('a', 10.0, 20.0, b"Helvetica", 0),
        observed('b', 40.0, 20.0, b"Helvetica", 0),
        observed('c', 45.0, 45.0, b"Helvetica", 0),
        observed('d', 50.0, 45.0, b"Times-Roman", 0),
    ]);
    let runs = glyph_runs(&characters);
    assert_eq!(runs.len(), 4);
    assert_eq!(
        runs.iter()
            .flat_map(|r| &r.words)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c", "d"]
    );
    assert_eq!(
        runs.iter().map(|r| r.text.as_str()).collect::<String>(),
        "abcd"
    );
    validate(&envelope(characters)).unwrap();
}

#[test]
fn positioned_spaces_and_geometryless_spaces_keep_boundaries_without_fake_word_boxes() {
    let characters = source(vec![
        observed('a', 10.0, 20.0, b"Helvetica", 0),
        geometryless(' ', false),
        observed(' ', 20.0, 20.0, b"Helvetica", 0),
        observed('b', 25.0, 20.0, b"Helvetica", 0),
    ]);
    let runs = glyph_runs(&characters);
    assert_eq!(runs[0].text, "a  b");
    assert_eq!(runs[0].words.len(), 2);
    assert_eq!(runs[0].words[0].width, 5.0);
    assert_eq!(runs[0].words[1].width, 5.0);
    validate(&envelope(characters)).unwrap();
}

#[test]
fn font_zero_length_empty_and_non_utf8_are_distinct_typed_states() {
    let absent = font_state(None);
    assert!(matches!(absent,FeatureState::Unsupported(ref r) if r==NO_FONT));
    for (bytes, reason) in [
        (vec![], "pdfium_font_empty_name"),
        (vec![255, 254], "pdfium_font_name_non_utf8"),
    ] {
        let mut o = observed('a', 10.0, 20.0, b"unused", 0);
        o.font = font_state(Some((bytes.clone(), 32)));
        let characters = source(vec![o]);
        let FeatureState::Present(info) = &characters[0].font else {
            panic!("exact raw data retained")
        };
        assert_eq!(info.raw_name, bytes);
        assert_eq!(info.flags, 32);
        assert!(matches!(&info.name,FeatureState::Unsupported(r) if r==reason));
        let runs = glyph_runs(&characters);
        assert!(runs[0].font_name.is_none());
        assert_eq!(runs[0].font_flags, Some(32));
        assert!(matches!(
            capabilities(&characters),
            (FeatureState::Unsupported(_), FeatureState::Present(_))
        ));
        validate(&envelope(characters)).unwrap();
    }
    let mut o = observed('a', 10.0, 20.0, b"unused", 0);
    o.font = absent;
    let characters = source(vec![o]);
    let runs = glyph_runs(&characters);
    assert!(runs[0].font_name.is_none() && runs[0].font_flags.is_none());
    validate(&envelope(characters)).unwrap();
}

#[test]
fn unavailable_fonts_never_merge_into_invented_shared_identity() {
    let mut observations = vec![
        observed('a', 10.0, 20.0, b"unused", 0),
        observed('b', 15.0, 20.0, b"unused", 0),
    ];
    for o in &mut observations {
        o.font = font_state(None);
    }
    let characters = source(observations);
    assert_eq!(glyph_runs(&characters).len(), 2);
    validate(&envelope(characters)).unwrap();
}

#[test]
fn zero_native_count_is_explicitly_empty_and_distinct_from_whitespace_stream() {
    let empty = source(vec![]);
    assert!(matches!(
        capabilities(&empty),
        (FeatureState::Absent, FeatureState::Absent)
    ));
    let value = envelope(empty);
    assert_eq!(value["native_text_metadata"]["native_char_count"], 0);
    validate(&value).unwrap();
}

#[test]
fn retention_seam_preserves_leading_missing_and_zero_area_whitespace() {
    let mut zero = geometryless('\u{a0}', false);
    zero.bounds = Some(NativeBounds {
        x: 4.0,
        y: 5.0,
        width: 0.0,
        height: 10.0,
    });
    let characters = source(vec![
        geometryless(' ', false),
        zero,
        observed('A', 10.0, 20.0, b"Helvetica", 0),
    ]);
    assert_eq!(characters.len(), 3);
    assert_eq!(
        characters.iter().map(character).collect::<String>(),
        " \u{a0}A"
    );
    assert!(matches!(characters[0].geometry, NativeGeometry::MissingBox));
    assert!(matches!(
        characters[1].geometry,
        NativeGeometry::ZeroArea(_)
    ));
    assert_eq!(
        characters.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    let runs = glyph_runs(&characters);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "A");
    assert_eq!(runs[0].x, 10.0);
    let bytes = serde_json::to_vec(&envelope(characters)).unwrap();
    validate_native_text_metadata(&bytes, &summary(), 1, &ResourceBudget::default()).unwrap();
    let retained: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        retained["native_text_metadata"]["characters"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn retention_seam_preserves_all_whitespace_without_claiming_no_native_characters() {
    let characters = source(vec![
        geometryless(' ', false),
        geometryless('\t', true),
        geometryless('\u{a0}', false),
        geometryless('\r', false),
        geometryless('\n', true),
    ]);
    assert_eq!(characters.len(), 5);
    assert_eq!(
        characters.iter().map(character).collect::<String>(),
        " \t\u{a0}\r\n"
    );
    assert!(glyph_runs(&characters).is_empty());
    let value = envelope(characters);
    assert_eq!(value["native_text_metadata"]["native_char_count"], 5);
    assert_eq!(value["capabilities"]["font_names"]["state"], "present");
    assert_eq!(
        value["capabilities"]["word_segmentation"]["state"],
        "absent"
    );
    assert_eq!(value["capabilities"]["native_stream"]["state"], "present");
    validate(&value).unwrap();
}

#[test]
fn retention_seam_keeps_explicit_and_generated_crlf_and_breaks_only_normalized_runs() {
    let mut explicit = geometryless('\r', false);
    explicit.char_code = 70;
    explicit.has_unicode_map_error = true;
    let mut generated = geometryless('\n', true);
    generated.char_code = 71;
    let characters = source(vec![
        observed('A', 10.0, 20.0, b"Helvetica", 0),
        explicit,
        generated,
        observed('B', 15.0, 20.0, b"Helvetica", 0),
    ]);
    assert_eq!(
        characters.iter().map(character).collect::<String>(),
        "A\r\nB"
    );
    assert_eq!(characters[1].char_code, 70);
    assert!(characters[1].has_unicode_map_error && !characters[1].generated);
    assert_eq!(characters[2].char_code, 71);
    assert!(characters[2].generated);
    assert_eq!(
        glyph_runs(&characters)
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>(),
        ["A", "B"]
    );
    validate(&envelope(characters)).unwrap();
}

#[test]
fn retention_seam_keeps_trailing_space_codes_errors_and_generated_distinction() {
    for last_generated in [false, true] {
        let mut first = geometryless(' ', true);
        first.char_code = 101;
        first.has_unicode_map_error = true;
        let mut last = geometryless(' ', last_generated);
        last.char_code = 102;
        let mut a = observed('A', 10.0, 20.0, b"Helvetica", 0);
        a.char_code = 100;
        let characters = source(vec![a, first, last]);
        let runs = glyph_runs(&characters);
        assert_eq!(runs[0].text, "A  ");
        assert_eq!(runs[0].char_codes, [100, 101, 102]);
        assert!(runs[0].has_unicode_map_error);
        assert_eq!(runs[0].trailing_space_generated, last_generated);
        assert!(characters[1].generated);
        assert_eq!(characters[2].generated, last_generated);
        assert_eq!(runs[0].words[0].text, "A");
        assert_eq!(runs[0].words[0].width, 5.0);
        validate(&envelope(characters)).unwrap();
    }
}

#[test]
fn retention_seam_preserves_invalid_raw_codepoint_without_lossy_font_or_geometry_recovery() {
    let mut o = observed('A', 10.0, 20.0, b"Helvetica", 0);
    o.unicode = u32::MAX;
    o.char_code = 987;
    o.has_unicode_map_error = true;
    let characters = source(vec![o]);
    assert_eq!(characters[0].unicode, u32::MAX);
    assert_eq!(glyph_runs(&characters)[0].text, "\u{fffd}");
    validate(&envelope(characters)).unwrap();
    for bounds in [
        None,
        Some(NativeBounds {
            x: 10.0,
            y: 20.0,
            width: 0.0,
            height: 10.0,
        }),
    ] {
        let mut o = observed('A', 10.0, 20.0, b"Helvetica", 0);
        o.bounds = bounds;
        let mut once = Some(o);
        assert!(
            collect_characters(
                1,
                1500.0,
                300.0,
                &ResourceBudget::default(),
                NativeBoundsPolicy::LegacyViewport,
                |_| Ok(once.take().unwrap())
            )
            .is_err()
        );
    }
}

#[test]
fn validator_rejects_missing_reordered_or_excluded_character_ordinals() {
    let baseline = envelope(source(vec![
        geometryless(' ', false),
        observed('A', 10.0, 20.0, b"Helvetica", 0),
        geometryless('\r', false),
        geometryless('\n', true),
        geometryless(' ', true),
    ]));
    validate(&baseline).unwrap();
    for case in 0..7 {
        let mut v = baseline.clone();
        let chars = &mut v["native_text_metadata"]["characters"];
        match case {
            0 => {
                chars.as_array_mut().unwrap().remove(0);
            }
            1 => {
                chars.as_array_mut().unwrap().remove(2);
            }
            2 => {
                chars.as_array_mut().unwrap().pop();
            }
            3 => chars[0]["ordinal"] = json!(1),
            4 => chars[3]["ordinal"] = json!(2),
            5 => chars[0]["excluded"] = json!(true),
            6 => chars[2]["unicode"] = json!('X' as u32),
            _ => unreachable!(),
        }
        assert!(validate(&v).is_err(), "ordinal/exclusion case {case}");
    }
}

#[test]
fn validator_rejects_forged_source_font_words_geometry_and_availability() {
    let baseline = envelope(source(vec![
        observed('a', 10.0, 20.0, b"Helvetica", 0),
        geometryless(' ', false),
        observed('b', 20.0, 20.0, b"Helvetica", 0),
    ]));
    validate(&baseline).unwrap();
    for case in 0..12 {
        let mut v = baseline.clone();
        match case {
            0 => v["native_text_metadata"]["source_sha256"] = json!("0".repeat(64)),
            1 => v["native_text_metadata"]["prepared_sha256"] = json!("0".repeat(64)),
            2 => {
                v["native_text_metadata"]["characters"][0]["font"]["value"]["name"] =
                    json!({"state":"present","value":"forged"})
            }
            3 => {
                v["native_text_metadata"]["characters"][0]["font"]["value"]["raw_name"] = json!([0])
            }
            4 => v["page"]["text_items"][0]["words"][0]["width"] = json!(900),
            5 => v["page"]["text_items"][0]["words"][0]["text"] = json!("made-up"),
            6 => v["native_text_metadata"]["characters"][0]["geometry"]["bounds"]["x"] = json!(-2),
            7 => {
                v["native_text_metadata"]["characters"][0]["font_size"] =
                    json!({"state":"present","value":0})
            }
            8 => v["capabilities"]["font_names"] = json!({"state":"absent"}),
            9 => v["native_text_metadata"]["layout_recipe"] = json!("controls implicitly excluded"),
            10 => v["native_text_metadata"]["word_recipe"] = json!("linguistic words"),
            11 => v["capabilities"]["native_stream"] = json!({"state":"absent"}),
            _ => unreachable!(),
        }
        assert!(validate(&v).is_err(), "forgery case {case}");
    }
}

#[test]
fn validation_and_retention_count_byte_and_working_bounds_reject_before_allocation_or_observation()
{
    let mut observed_count = 0;
    assert!(
        collect_characters(
            u32::MAX,
            1500.0,
            300.0,
            &ResourceBudget::default(),
            NativeBoundsPolicy::LegacyViewport,
            |_| {
                observed_count += 1;
                Ok(geometryless(' ', false))
            }
        )
        .is_err()
    );
    assert_eq!(observed_count, 0);
    let baseline = envelope(source(vec![observed('a', 10.0, 20.0, b"Helvetica", 0)]));
    let bytes = serde_json::to_vec(&baseline).unwrap();
    let mut budget = ResourceBudget::default();
    budget.page_layout_bytes = bytes.len() as u64 - 1;
    assert!(validate_native_text_metadata(&bytes, &summary(), 1, &budget).is_err());
    let mut budget = ResourceBudget::default();
    budget.owned_working_bytes = 64 * bytes.len() as u64 - 1;
    assert!(validate_native_text_metadata(&bytes, &summary(), 1, &budget).is_err());
}

#[test]
fn v6_profile_binds_full_stream_and_layout_policies_without_changing_v5() {
    let s = summary();
    let v5 = crate::document_metadata_native_profile_digest(&s, 96.0, false).unwrap();
    let v6 = native_font_word_profile_digest(&s, 96.0, false).unwrap();
    assert_ne!(v5, v6);
    assert_eq!(
        v6,
        native_font_word_profile_digest(&s, 96.0, false).unwrap()
    );
    assert_ne!(
        v6,
        native_font_word_profile_digest(&s, 72.0, false).unwrap()
    );
    assert_ne!(v6, native_font_word_profile_digest(&s, 96.0, true).unwrap());
}

#[test]
fn exact_font_inputs_restore_existing_per_font_corruption_detection() {
    let healthy = "Clear audio and ordinary ocean stories are readable. ".repeat(4);
    let corrupt = "bcdfghjklmnpqrstvwxyz".repeat(3);
    let mut observations = Vec::new();
    for (row, (text, font)) in [
        (healthy.as_str(), b"Helvetica".as_slice()),
        (corrupt.as_str(), b"Courier".as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        for (column, c) in text.chars().enumerate() {
            observations.push(observed(
                c,
                10.0 + column as f32 * 5.0,
                20.0 + row as f32 * 20.0,
                font,
                0,
            ));
        }
    }
    let characters = source(observations);
    let raw = page(&characters);
    assert!(liteparse::ocr_merge::calculate_native_page_complexity(&raw, &[], 0, 1).is_garbled);
    let mut missing = raw.clone();
    for item in &mut missing.text_items {
        item.font_name = None;
    }
    assert!(
        !liteparse::ocr_merge::calculate_native_page_complexity(&missing, &[], 0, 1).is_garbled
    );
    validate(&envelope(characters)).unwrap();
}

#[tokio::test]
async fn synthetic_pdf_preserves_actual_fonts_words_and_complete_native_ordinals() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let content =
        "BT /F1 12 Tf 30 180 Td (  Alpha ) Tj /F2 12 Tf (Beta) Tj 0 -24 Td (Gamma ) Tj ET";
    let objects=[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 220] /Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream",content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),"<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman >>".into(),
    ];
    let path = directory.path().join("fonts.pdf");
    std::fs::write(&path, crate::tests::pdf_objects(&objects))?;
    let budget = ResourceBudget::default();
    let prepared = crate::prepare_document(
        &path,
        &directory.path().join("staging"),
        &crate::PreparationOptions,
        &budget,
    )
    .await?;
    let candidate = crate::extract_native(
        &prepared,
        &crate::NativeOptions {
            render_pages: false,
            ..Default::default()
        },
        &budget,
    )
    .await?;
    let raw = &candidate.raw_pages[0];
    assert!(
        raw.text_items
            .iter()
            .any(|item| item.font_name.as_deref() == Some("Helvetica"))
    );
    assert!(
        raw.text_items
            .iter()
            .any(|item| item.font_name.as_deref() == Some("Times-Roman"))
    );
    assert_eq!(
        raw.text_items
            .iter()
            .flat_map(|r| &r.words)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "Beta", "Gamma"]
    );
    let a = &candidate.pages()[0].metadata_artifact;
    let bytes = std::fs::read(prepared.staging_root().join(&a.relative_path))?;
    crate::validate_native_graphics_metadata(
        &bytes,
        prepared.summary(),
        candidate.native_profile_sha256(),
        1,
        &budget,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let count = value["native_text_metadata"]["native_char_count"]
        .as_u64()
        .unwrap();
    let characters = value["native_text_metadata"]["characters"]
        .as_array()
        .unwrap();
    assert_eq!(characters.len() as u64, count);
    for (index, c) in characters.iter().enumerate() {
        assert_eq!(c["ordinal"], index);
    }
    let final_document = crate::finalize_document(candidate, vec![], crate::OcrMode::Off, &budget)?;
    assert!(
        final_document.document.text.contains("Alpha")
            && final_document.document.text.contains("Gamma")
    );
    Ok(())
}

#[test]
fn positioned_font_size_refuses_f64_to_f32_underflow_and_preserves_representable_raw_size() {
    let tiny = f64::from_bits(1);
    assert!(tiny > 0.0 && tiny.is_finite());
    assert_eq!(tiny as f32, 0.0);
    let mut invalid = observed('A', 10.0, 20.0, b"Helvetica", 0);
    invalid.font_size = tiny;
    let mut invalid = Some(invalid);
    assert!(
        collect_characters(
            1,
            1500.0,
            300.0,
            &ResourceBudget::default(),
            NativeBoundsPolicy::LegacyViewport,
            |_| Ok(invalid.take().unwrap())
        )
        .is_err()
    );
    let raw = 12.0000000001_f64;
    let mut valid = observed('A', 10.0, 20.0, b"Helvetica", 0);
    valid.font_size = raw;
    let characters = source(vec![valid]);
    assert!(
        matches!(characters[0].font_size, FeatureState::Present(size) if size.to_bits() == raw.to_bits())
    );
    assert_eq!(glyph_runs(&characters)[0].font_size, Some(raw as f32));
    let mut forged = envelope(characters);
    forged["native_text_metadata"]["characters"][0]["font_size"]["value"] = json!(tiny);
    assert!(validate(&forged).is_err());
    let mut control = geometryless('\n', true);
    control.font_size = tiny;
    let controls = source(vec![control]);
    assert!(
        matches!(controls[0].font_size, FeatureState::Present(size) if size.to_bits() == tiny.to_bits())
    );
    assert!(glyph_runs(&controls).is_empty());
    validate(&envelope(controls)).unwrap();
}

// Four known PDF viewport maps are written here directly from this fixture's
// authored CropBox and UserUnit. The unrotated native outline is an observed
// PDFium reference for the chosen standard font, not an invented font metric.
// This oracle never invokes the production source_to_page helper for expected
// geometry; all transformed word boxes must match independently mapped corners.
#[tokio::test]
async fn synthetic_rotated_cropped_userunit_words_match_independent_viewport_oracle() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let content = "BT /F1 12 Tf 50 100 Td (AB) Tj ET";
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R 7 0 R] /Count 5 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 220] /Resources << /Font << /F1 9 0 R >> >> /Contents 8 0 R >>".into(),
    ];
    for degrees in [0, 90, 180, 270] {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 220] /CropBox [10 20 290 200] /UserUnit 2 /Rotate {degrees} /Resources << /Font << /F1 9 0 R >> >> /Contents 8 0 R >>"));
    }
    objects.push(format!(
        "<< /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    ));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    let path = directory.path().join("rotated-fonts.pdf");
    std::fs::write(&path, crate::tests::pdf_objects(&objects))?;
    let budget = ResourceBudget::default();
    let prepared = crate::prepare_document(
        &path,
        &directory.path().join("stage"),
        &crate::PreparationOptions,
        &budget,
    )
    .await?;
    let candidate = crate::extract_native(
        &prepared,
        &crate::NativeOptions {
            render_pages: false,
            ..Default::default()
        },
        &budget,
    )
    .await?;
    let mut metadata = Vec::new();
    for page in candidate.pages() {
        let bytes = std::fs::read(
            prepared
                .staging_root()
                .join(&page.metadata_artifact.relative_path),
        )?;
        crate::validate_native_graphics_metadata(
            &bytes,
            prepared.summary(),
            candidate.native_profile_sha256(),
            page.number,
            &budget,
        )?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        let stream: NativeTextMetadata = serde_json::from_value(v["native_text_metadata"].clone())?;
        assert_eq!(stream.characters.len(), stream.native_char_count as usize);
        assert!(
            stream
                .characters
                .iter()
                .enumerate()
                .all(|(i, c)| c.ordinal == i as u32)
        );
        metadata.push(stream);
    }
    let baseline = &metadata[0].characters;
    for (quarter, page) in candidate.pages()[1..].iter().enumerate() {
        let expected_size = if quarter % 2 == 0 {
            (560.0, 360.0)
        } else {
            (360.0, 560.0)
        };
        assert_eq!((page.width_points, page.height_points), expected_size);
        assert_eq!(page.geometry.rotation_quarter_turns, quarter as i32);
        assert_eq!(page.geometry.user_unit, 2.0);
        let map = |x: f64, y: f64| match quarter {
            0 => (2.0 * (x - 10.0), 2.0 * (200.0 - y)),
            1 => (2.0 * (y - 20.0), 2.0 * (x - 10.0)),
            2 => (2.0 * (290.0 - x), 2.0 * (y - 20.0)),
            3 => (2.0 * (200.0 - y), 2.0 * (290.0 - x)),
            _ => unreachable!(),
        };
        let expected_point = map(50.0, 100.0);
        let actual_point = page.geometry.source_to_page.apply(50.0, 100.0);
        assert!(
            (actual_point.0 - expected_point.0).abs() < 0.001
                && (actual_point.1 - expected_point.1).abs() < 0.001
        );
        let inverse = page
            .geometry
            .page_to_source
            .apply(expected_point.0, expected_point.1);
        assert!((inverse.0 - 50.0).abs() < 0.001 && (inverse.1 - 100.0).abs() < 0.001);
        let raw = &candidate.raw_pages[quarter + 1];
        assert_eq!(
            raw.text_items
                .iter()
                .flat_map(|r| &r.words)
                .map(|w| w.text.as_str())
                .collect::<String>(),
            "AB"
        );
        for word in raw.text_items.iter().flat_map(|r| &r.words) {
            let mut points = Vec::new();
            for letter in word.text.chars() {
                let original = baseline
                    .iter()
                    .find(|c| c.unicode == letter as u32)
                    .unwrap();
                let NativeGeometry::Positioned(ref b) = original.geometry else {
                    panic!("reference glyph positioned")
                };
                // Reference page has fullMediaBox, rotation0 and UserUnit1;
                // viewport top220 flips PDF source y, independently inverted.
                for (x, y) in [
                    (b.x, b.y),
                    (b.x + b.width, b.y),
                    (b.x, b.y + b.height),
                    (b.x + b.width, b.y + b.height),
                ] {
                    points.push(map(f64::from(x), 220.0 - f64::from(y)));
                }
            }
            let left = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
            let right = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
            let top = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
            let bottom = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
            for (actual, expected) in [
                (word.x, left),
                (word.y, top),
                (word.width, right - left),
                (word.height, bottom - top),
            ] {
                assert!(
                    (f64::from(actual) - expected).abs() < 0.001,
                    "rotation{quarter} word{} actual{actual} expected{expected}",
                    word.text
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn synthetic_v6_complete_metadata_and_document_signals_are_window_1_25_invariant()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let count = 27;
    let font = 3 + count * 2;
    let kids = (0..count)
        .map(|i| format!("{} 0 R", 3 + i * 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>"),
    ];
    for i in 0..count {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 220] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {} 0 R >>",4+i*2));
        let content = format!(
            "BT /F1 12 Tf 30 180 Td (Native page {} Alpha Beta) Tj ET",
            i + 1
        );
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    let path = directory.path().join("windows.pdf");
    std::fs::write(&path, crate::tests::pdf_objects(&objects))?;
    let budget = ResourceBudget::default();
    let first = crate::prepare_document(
        &path,
        &directory.path().join("stage1"),
        &crate::PreparationOptions,
        &budget,
    )
    .await?;
    let second = crate::prepare_document(
        &path,
        &directory.path().join("stage25"),
        &crate::PreparationOptions,
        &budget,
    )
    .await?;
    let a = crate::extract_native(
        &first,
        &crate::NativeOptions {
            render_pages: false,
            page_window: 1,
            ..Default::default()
        },
        &budget,
    )
    .await?;
    let b = crate::extract_native(
        &second,
        &crate::NativeOptions {
            render_pages: false,
            page_window: 25,
            ..Default::default()
        },
        &budget,
    )
    .await?;
    assert_eq!(a.pages().len(), 27);
    assert_eq!(a.native_profile_sha256(), b.native_profile_sha256());
    for (ap, bp) in a.pages().iter().zip(b.pages()) {
        let av = std::fs::read(
            first
                .staging_root()
                .join(&ap.metadata_artifact.relative_path),
        )?;
        let bv = std::fs::read(
            second
                .staging_root()
                .join(&bp.metadata_artifact.relative_path),
        )?;
        crate::validate_native_graphics_metadata(
            &av,
            first.summary(),
            a.native_profile_sha256(),
            ap.number,
            &budget,
        )?;
        crate::validate_native_graphics_metadata(
            &bv,
            second.summary(),
            b.native_profile_sha256(),
            bp.number,
            &budget,
        )?;
        assert_eq!(
            av, bv,
            "exact v7 metadata includes complete font/character/word stream"
        );
        assert_eq!(ap.metadata_artifact.sha256, bp.metadata_artifact.sha256);
        assert_eq!(
            serde_json::to_value(&ap.complexity)?,
            serde_json::to_value(&bp.complexity)?
        );
    }
    let a = crate::finalize_document(a, vec![], crate::OcrMode::Off, &budget)?;
    let b = crate::finalize_document(b, vec![], crate::OcrMode::Off, &budget)?;
    assert_eq!(
        serde_json::to_value(&a.document)?,
        serde_json::to_value(&b.document)?
    );
    assert_eq!(
        serde_json::to_value(&a.pages)?,
        serde_json::to_value(&b.pages)?
    );
    assert_eq!(a.unmapped_merged_lines, b.unmapped_merged_lines);
    Ok(())
}
