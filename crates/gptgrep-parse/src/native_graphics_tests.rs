use super::*;
use std::cell::Cell;

fn summary() -> PreparedSummary {
    PreparedSummary {
        source_sha256: "1".repeat(64),
        prepared_sha256: "2".repeat(64),
        source_bytes: 100,
        prepared_bytes: 100,
        format: "pdf".into(),
        parser_identity: crate::RICH_PARSER.into(),
        preparation_profile_sha256: "3".repeat(64),
        conversion: FeatureState::Absent,
        image_geometry: FeatureState::Absent,
    }
}
fn geometry() -> PageGeometry {
    let t = AffineTransform {
        a: 2.,
        b: 0.,
        c: 0.,
        d: -2.,
        e: -20.,
        f: 300.,
    };
    PageGeometry {
        width_points: 400.,
        height_points: 300.,
        rotation_quarter_turns: 0,
        user_unit: 2.,
        source_to_page: t,
        page_to_source: t.inverse().unwrap(),
    }
}
#[test]
fn native_graphics_pdf_frame_qualifies_rotations_units_and_adapter_precision() -> Result<()> {
    for unit in [1.0, 2.0, f64::from(1.1_f32)] {
        for (rotation, [a, b, c, d]) in [
            [unit, 0.0, 0.0, -unit],
            [0.0, unit, unit, 0.0],
            [-unit, 0.0, 0.0, unit],
            [0.0, -unit, -unit, 0.0],
        ]
        .into_iter()
        .enumerate()
        {
            let transform = AffineTransform {
                a,
                b,
                c,
                d,
                e: -20.0,
                f: 30.0,
            };
            let mut frame = PageGeometry {
                width_points: 240.0,
                height_points: 300.0,
                rotation_quarter_turns: rotation as i32,
                user_unit: unit,
                source_to_page: transform,
                page_to_source: transform.inverse()?,
            };
            frame.validate_native_pdf()?;
            let mut forged = frame.clone();
            forged.user_unit = 3.0;
            assert!(forged.validate_native_pdf().is_err());
            let mut forged = frame.clone();
            forged.rotation_quarter_turns = (rotation as i32 + 1) % 4;
            assert!(forged.validate_native_pdf().is_err());
            // A sub-device-quantum f32 perturbation is accepted; the recipe
            // does not demand exact equality to the ideal mathematical slope.
            if frame.source_to_page.a != 0.0 {
                frame.source_to_page.a = f64::from(frame.source_to_page.a as f32 + 0.0005_f32);
            } else {
                frame.source_to_page.b = f64::from(frame.source_to_page.b as f32 + 0.0005_f32);
            }
            frame.page_to_source = frame.source_to_page.inverse()?;
            frame.validate_native_pdf()?;
        }
    }
    let mut generic = geometry();
    generic.user_unit = 1.0;
    generic.source_to_page = AffineTransform::IDENTITY;
    generic.page_to_source = AffineTransform::IDENTITY;
    generic.validate()?;
    assert!(generic.validate_native_pdf().is_err());
    let mut coarse = geometry();
    coarse.source_to_page.e = f64::from(1.0e20_f32);
    coarse.page_to_source = coarse.source_to_page.inverse()?;
    assert!(coarse.validate_native_pdf().is_err());
    Ok(())
}
#[derive(Clone)]
struct Node {
    value: ObjectObservation,
    children: Option<Vec<usize>>,
    segments: Option<Vec<SegmentObservation>>,
}
fn generic(id: usize, kind: i32) -> Node {
    Node {
        value: ObjectObservation {
            identity: id,
            raw_kind: kind,
            matrix: Some([1., 0., 0., 1., 0., 0.]),
            bounds: Some([-4., -3., 12., 16.]),
            segments: None,
            draw: None,
            stroke_width: None,
            fill: None,
            stroke: None,
            image: None,
        },
        children: None,
        segments: None,
    }
}
fn path(id: usize) -> Node {
    let mut n = generic(id, 2);
    n.segments = Some(vec![
        seg(NativeSegmentKind::MoveTo, [0., 0.], false),
        seg(NativeSegmentKind::LineTo, [10., 0.], false),
        seg(NativeSegmentKind::LineTo, [10., 10.], true),
    ]);
    n.value.segments = Some(3);
    n.value.draw = Some((2, true));
    n.value.stroke_width = Some(2.);
    n.value.fill = Some([1, 2, 3, 4]);
    n.value.stroke = Some([5, 6, 7, 8]);
    n
}
fn seg(kind: NativeSegmentKind, p: [f32; 2], close: bool) -> SegmentObservation {
    SegmentObservation {
        kind: Some(kind),
        point: Some(p),
        close,
    }
}
fn image(id: usize) -> Node {
    let mut n = generic(id, 3);
    n.value.matrix = Some([0., 2., -3., 1., 4., 5.]);
    n.value.image = Some(ImageObservation {
        width: 2,
        height: 3,
        bits: 8,
        colorspace: 1,
        marked: -1,
        horizontal_dpi: 72.,
        vertical_dpi: 96.,
    });
    n
}
fn form(id: usize, children: Vec<usize>, m: [f32; 6]) -> Node {
    let mut n = generic(id, 5);
    n.value.matrix = Some(m);
    n.children = Some(children);
    n
}
fn collect_nodes(
    nodes: &[Node],
    roots: Option<&[usize]>,
    limit: u64,
    gets: &Cell<usize>,
) -> Result<GraphicsExtraction> {
    collect(
        &summary(),
        &"4".repeat(64),
        1,
        &geometry(),
        limit,
        |index| {
            gets.set(gets.get() + 1);
            roots
                .and_then(|r| r.get(index))
                .copied()
                .filter(|i| *i < nodes.len())
        },
        |parent: Option<&usize>| match parent {
            None => roots.map(<[usize]>::len),
            Some(i) => nodes[*i].children.as_ref().map(Vec::len),
        },
        |parent, index| {
            gets.set(gets.get() + 1);
            nodes[*parent]
                .children
                .as_ref()
                .and_then(|r| r.get(index))
                .copied()
                .filter(|i| *i < nodes.len())
        },
        |i| nodes[*i].value,
        |i, s| nodes[*i].segments.as_ref().and_then(|v| v.get(s)).copied(),
        |_| Ok(()),
    )
}
fn run(nodes: &[Node], roots: &[usize]) -> Result<GraphicsExtraction> {
    collect_nodes(nodes, Some(roots), 4_194_304, &Cell::new(0))
}
fn envelope_metadata(g: GraphicsExtraction) -> NativePageMetadata {
    let geometry = geometry();
    let (bounds, vectors) = derive_layout(&g.header, &g.objects).unwrap();
    let page = liteparse::types::Page {
        page_number: 1,
        page_label: None,
        page_width: 400.,
        page_height: 300.,
        content_bounds: bounds,
        text_items: vec![],
        graphics: vec![],
        vector_graphics: Some(vectors),
        struct_nodes: vec![],
        image_refs: vec![],
        annotations: None,
        form_fields: None,
        structure_tree: None,
    };
    let mut capabilities = BTreeMap::new();
    capabilities.insert(
        "native_recipe".into(),
        FeatureState::Present(NATIVE_GRAPHICS_RECIPE.into()),
    );
    capabilities.insert(
        "native_stream".into(),
        FeatureState::Present(crate::rich::native_text_metadata::stream_recipe().into()),
    );
    capabilities.insert("font_names".into(), FeatureState::Absent);
    capabilities.insert("word_segmentation".into(), FeatureState::Absent);
    capabilities.insert(
        "native_character_geometry".into(),
        FeatureState::Present(crate::rich::native_text_metadata::UNCLIPPED_GEOMETRY.into()),
    );
    capabilities.insert(
        "native_layout_graphics".into(),
        FeatureState::Present(LAYOUT_GRAPHICS_RECIPE.into()),
    );
    capabilities.insert(
        "native_content_bounds_semantics".into(),
        FeatureState::Present(CONTENT_BOUNDS_SEMANTICS.into()),
    );
    let complexity =
        liteparse::ocr_merge::calculate_native_page_complexity(&page, &g.image_rects, 0, 1);
    NativePageMetadata {
        page,
        objects: g.objects,
        capabilities,
        complexity,
        image_rects: g.image_rects,
        geometry,
        native_text_metadata: crate::rich::native_text_metadata::metadata(&summary(), 1, 0, vec![]),
        graphics: g.header,
    }
}
fn validate(m: &NativePageMetadata) -> Result<NativePageMetadata> {
    validate_native_graphics_metadata(
        &serde_json::to_vec(m)?,
        &summary(),
        &"4".repeat(64),
        1,
        &ResourceBudget::default(),
    )
}
#[test]
fn native_graphics_json_roundtrip_preserves_fractional_bits_and_rejects_one_ulp_forgery()
-> Result<()> {
    // A real widened-f32/affine result whose short decimal was one ULP off
    // under serde_json's default significand/power-of-ten conversion. This
    // assertion requires the selected float_roundtrip feature, not tolerance.
    let exact = 93.84700012207031_f64;
    let decoded: f64 = serde_json::from_slice(b"93.84700012207031")?;
    assert_eq!(decoded.to_bits(), exact.to_bits());
    let mut nodes = vec![
        path(10),
        form(11, vec![2], [1.1, 0.2, 0.3, 0.9, 0.7, 1.3]),
        form(12, vec![3, 4], [0.7, -0.1, 0.2, 1.2, 3.4, -5.6]),
        path(13),
        image(14),
    ];
    nodes[0].value.bounds = Some([56.9235, -3.125, 100.2, 16.1]);
    nodes[3].value.matrix = Some([0.9, 0.3, -0.2, 1.1, 0.1, 0.7]);
    nodes[3].segments = Some(vec![
        seg(NativeSegmentKind::MoveTo, [0.1, 0.3], false),
        seg(NativeSegmentKind::LineTo, [4.7, 5.9], false),
        seg(NativeSegmentKind::LineTo, [7.1, 9.3], true),
    ]);
    let base = envelope_metadata(run(&nodes, &[0, 1])?);
    assert_eq!(
        base.objects[0].page_bounds_quad[0][0].to_bits(),
        exact.to_bits()
    );
    let bytes = serde_json::to_vec(&base)?;
    let decoded = validate_native_graphics_metadata(
        &bytes,
        &summary(),
        &"4".repeat(64),
        1,
        &ResourceBudget::default(),
    )?;
    assert_eq!(serde_json::to_vec(&decoded)?, bytes);
    for (before, after) in base.objects.iter().zip(&decoded.objects) {
        assert_eq!(
            before.native_bounds.map(f32::to_bits),
            after.native_bounds.map(f32::to_bits)
        );
        assert_eq!(
            before.ancestor_matrix.map(f64::to_bits),
            after.ancestor_matrix.map(f64::to_bits)
        );
        assert_eq!(
            before.page_bounds_quad.map(|p| p.map(f64::to_bits)),
            after.page_bounds_quad.map(|p| p.map(f64::to_bits))
        );
        if let (Some(before), Some(after)) = (&before.path, &after.path) {
            for (a, b) in before.segments.iter().zip(&after.segments) {
                assert_eq!(a.raw_point.map(f32::to_bits), b.raw_point.map(f32::to_bits));
                assert_eq!(
                    a.page_point.map(f64::to_bits),
                    b.page_point.map(f64::to_bits)
                );
            }
        }
    }
    // Every mutation changes one derived value by the smallest representable
    // positive increment. Raw observations remain unchanged; strict validation
    // must still refuse bounds, ancestor/local matrices, segments and images.
    for case in 0..7 {
        let mut forged = decoded.clone();
        match case {
            0 => {
                let value = &mut forged.objects[0].page_bounds_quad[0][0];
                *value = value.next_up();
            }
            1 => {
                let value = &mut forged.objects[0].page_bounds.left;
                *value = value.next_up();
            }
            2 => {
                let value = &mut forged.objects[3].path.as_mut().unwrap().segments[0].page_point[0];
                *value = value.next_up();
            }
            3 => {
                let FeatureState::Present(value) = &mut forged.objects[3].local_to_page else {
                    panic!("path matrix");
                };
                value[0] = value[0].next_up();
            }
            4 => {
                let value = &mut forged.objects[3].ancestor_matrix[0];
                *value = value.next_up();
            }
            5 => {
                let value = &mut forged.objects[4].image.as_mut().unwrap().page_quad[0][0];
                *value = value.next_up();
            }
            _ => {
                let value = &mut forged.objects[4].image.as_mut().unwrap().page_envelope.left;
                *value = value.next_up();
            }
        }
        assert!(validate(&forged).is_err(), "one-ULP forgery {case}");
    }
    Ok(())
}
#[test]
fn native_graphics_nested_noncommuting_forms_paths_images_and_four_corner_bounds() -> Result<()> {
    let mut nodes = vec![
        form(10, vec![1], [0., 1., -1., 0., 30., 40.]),
        form(11, vec![2, 3], [2., 0.5, 1., 3., 5., 6.]),
        path(12),
        image(13),
    ];
    nodes[2].value.matrix = Some([1., 0., 0., 1., 7., 8.]);
    let g = run(&nodes, &[0])?;
    assert_eq!(
        g.objects
            .iter()
            .map(|o| o.ordinal_path.clone())
            .collect::<Vec<_>>(),
        vec![vec![0], vec![0, 0], vec![0, 0, 0], vec![0, 0, 1]]
    );
    let p = g.objects[2].path.as_ref().unwrap();
    // Independently calculated V(A0(A1(M(0,0)))): (7,8)->(27,33.5)->(-3.5,67)->(-27,166).
    assert_eq!(p.segments[0].page_point, [-27., 166.]);
    assert_ne!(
        compose(
            matrix(nodes[0].value.matrix.unwrap())?,
            matrix(nodes[1].value.matrix.unwrap())?
        )?,
        compose(
            matrix(nodes[1].value.matrix.unwrap())?,
            matrix(nodes[0].value.matrix.unwrap())?
        )?
    );
    let im = g.objects[3].image.as_ref().unwrap();
    assert_ne!(im.page_quad, g.objects[3].page_bounds_quad);
    let rect = g.objects[2].page_bounds;
    assert_eq!(rect, envelope(&g.objects[2].page_bounds_quad));
    assert_ne!(g.objects[2].page_bounds_quad[0], p.segments[0].page_point);
    validate(&envelope_metadata(g))?;
    Ok(())
}
#[test]
fn native_graphics_signed_zero_area_singular_and_generic_union() -> Result<()> {
    let mut p = path(1);
    p.value.matrix = Some([0., 0., 0., 0., -20., 30.]);
    p.value.bounds = Some([-20., 30., -20., 30.]);
    let mut shading = generic(2, 4);
    shading.value.matrix = None;
    shading.value.bounds = Some([-100., -50., 200., 250.]);
    let mut unknown = generic(3, 1234);
    unknown.value.matrix = None;
    unknown.value.bounds = Some([-300., -80., 0., 0.]);
    let g = run(&[p, shading, unknown], &[0, 1, 2])?;
    assert_eq!(
        g.objects[0].page_bounds.left,
        g.objects[0].page_bounds.right
    );
    assert!(matches!(
        g.objects[1].local_to_page,
        FeatureState::Unsupported(_)
    ));
    let FeatureState::Present(r) = g.header.content_bounds else {
        panic!("extent")
    };
    assert_eq!(
        r,
        NativeRect {
            left: -620.,
            right: 380.,
            bottom: -200.,
            top: 460.
        }
    );
    assert_eq!(g.header.top_level_count, 3);
    validate(&envelope_metadata(g))?;
    let empty = run(&[], &[])?;
    assert!(matches!(empty.header.content_bounds, FeatureState::Absent));
    assert!(empty.header.zero_count_provenance.contains("error"));
    Ok(())
}
#[test]
fn native_graphics_required_getter_failures_reject_before_partial_publication() {
    let gets = Cell::new(0);
    assert!(collect_nodes(&[], None, 4_194_304, &gets).is_err());
    assert_eq!(gets.get(), 0);
    assert!(run(&[], &[0]).is_err());
    for failure in 0..7 {
        let mut p = path(1);
        match failure {
            0 => p.value.bounds = None,
            1 => p.value.matrix = None,
            2 => p.value.segments = None,
            3 => p.segments.as_mut().unwrap()[1].point = None,
            4 => p.segments.as_mut().unwrap()[1].kind = None,
            5 => p.segments.as_mut().unwrap().pop().map(|_| ()).unwrap(),
            _ => p.value.draw = None,
        };
        assert!(run(&[p], &[0]).is_err(), "failure {failure}");
    }
    let mut f = form(1, vec![], [1., 0., 0., 1., 0., 0.]);
    f.children = None;
    assert!(run(&[f], &[0]).is_err());
    let mut i = image(1);
    i.value.image = None;
    assert!(run(&[i], &[0]).is_err());
}
#[test]
fn native_graphics_source_order_repeated_branches_cycle_and_depth_boundary() -> Result<()> {
    let nodes = vec![
        form(1, vec![2], [1., 0., 0., 1., 0., 0.]),
        form(2, vec![2], [1., 0., 0., 1., 50., 0.]),
        path(3),
    ];
    let g = run(&nodes, &[0, 1])?;
    assert_eq!(g.objects.len(), 4);
    assert_ne!(g.objects[1].id, g.objects[3].id);
    assert_ne!(
        g.objects[1].path.as_ref().unwrap().segments[0].page_point,
        g.objects[3].path.as_ref().unwrap().segments[0].page_point
    );
    let cycle = vec![form(1, vec![0], [1., 0., 0., 1., 0., 0.])];
    assert!(run(&cycle, &[0]).unwrap_err().to_string().contains("cycle"));
    let mut deep: Vec<_> = (0..MAX_DEPTH - 1)
        .map(|i| form(i, vec![i + 1], [1., 0., 0., 1., 0., 0.]))
        .collect();
    deep.push(path(MAX_DEPTH - 1));
    assert_eq!(run(&deep, &[0])?.objects.len(), MAX_DEPTH);
    deep[MAX_DEPTH - 1] = form(MAX_DEPTH - 1, vec![MAX_DEPTH], [1., 0., 0., 1., 0., 0.]);
    deep.push(path(MAX_DEPTH));
    assert!(run(&deep, &[0]).is_err());
    Ok(())
}
#[test]
fn native_graphics_curves_fill_rules_closes_and_unsupported_paint() -> Result<()> {
    for fill in 0..=2 {
        let mut p = path(1);
        p.value.draw = Some((fill, fill != 0));
        p.value.matrix = Some([2., 0., 1., 3., 0., 0.]);
        p.value.stroke = None;
        p.segments = Some(vec![
            seg(NativeSegmentKind::MoveTo, [0., 0.], false),
            seg(NativeSegmentKind::BezierTo, [1., 0.], false),
            seg(NativeSegmentKind::BezierTo, [2., 3.], false),
            seg(NativeSegmentKind::BezierTo, [4., 3.], true),
        ]);
        p.value.segments = Some(4);
        let g = run(&[p], &[0])?;
        let path = g.objects[0].path.as_ref().unwrap();
        assert_eq!(path.paint.fill_rule, fill_rule(fill)?);
        assert!(matches!(
            path.paint.stroke_rgba,
            FeatureState::Unsupported(_)
        ));
        assert!(matches!(path.paint.dash, FeatureState::Unsupported(_)));
        assert!(matches!(path.paint.clip, FeatureState::Unsupported(_)));
        let (_, v) = derive_layout(&g.header, &g.objects)?;
        assert!(v.shapes[0].has_curve);
        assert!(v.lines.iter().all(|l| l.stroke_width.is_none()));
        validate(&envelope_metadata(g))?;
    }
    let mut p = path(1);
    p.value.draw = Some((99, true));
    assert!(run(&[p], &[0]).is_err());
    let mut p = path(1);
    p.segments.as_mut().unwrap()[1].kind = Some(NativeSegmentKind::BezierTo);
    assert!(run(&[p], &[0]).is_err());
    Ok(())
}
#[test]
fn native_graphics_preallocation_limits_and_checked_overflow() {
    let gets = Cell::new(0);
    let p = path(1);
    assert!(collect_nodes(std::slice::from_ref(&p), Some(&[0]), 1, &gets).is_err());
    assert_eq!(gets.get(), 0);
    let exact = run(std::slice::from_ref(&p), &[0]).unwrap().charged_bytes;
    assert!(collect_nodes(std::slice::from_ref(&p), Some(&[0]), exact, &gets).is_ok());
    assert!(collect_nodes(std::slice::from_ref(&p), Some(&[0]), exact - 1, &gets).is_err());
    let mut many = p;
    many.value.segments = Some(usize::MAX);
    assert!(run(&[many], &[0]).is_err());
    let counter = Cell::new(0);
    let result = collect::<usize>(
        &summary(),
        &"4".repeat(64),
        1,
        &geometry(),
        4_194_304,
        |_| {
            counter.set(counter.get() + 1);
            None
        },
        |_| Some(usize::MAX),
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _| unreachable!(),
        |_| Ok(()),
    );
    assert!(result.is_err());
    assert_eq!(counter.get(), 0);
    let mut budget = ResourceBudget::default();
    budget.run_staging_bytes = 100;
    budget.reserve_retained_staging(80).unwrap();
    assert!(budget.reserve_retained_staging(21).is_err());
    assert_eq!(budget.clone().staging_used(), 80);
}
#[test]
fn native_graphics_validator_rejects_forged_geometry_counts_ids_paint_and_derivatives() -> Result<()>
{
    let base = envelope_metadata(run(&[path(1), image(2)], &[0, 1])?);
    validate(&base)?;
    for case in 0..22 {
        let mut m = base.clone();
        match case {
            0 => m.objects[0].path.as_mut().unwrap().segments[0].page_point[0] += 1.,
            1 => m.objects[1].image.as_mut().unwrap().page_quad[2][1] += 1.,
            2 => m.graphics.object_count += 1,
            3 => m.graphics.top_level_count += 1,
            4 => m.objects[0].id = "0".repeat(64),
            5 => m.objects[0].ordinal_path = vec![1],
            6 => m.objects[0].ancestor_matrix[4] = 1.,
            7 => m.objects[0].page_bounds.right += 1.,
            8 => m.graphics.content_bounds = FeatureState::Absent,
            9 => m.objects[0].path.as_mut().unwrap().paint.dash = FeatureState::Present(vec![1.]),
            10 => m.page.vector_graphics = None,
            11 => m.image_rects.clear(),
            12 => m.objects[0].child_count = Some(0),
            13 => m.graphics.page = 2,
            14 => m.graphics.source_sha256 = "0".repeat(64),
            15 => m.graphics.prepared_sha256 = "0".repeat(64),
            16 => m.graphics.native_profile_sha256 = "0".repeat(64),
            17 => m.graphics.content_bounds_semantics = "visible_pixels".into(),
            18 => m.objects[0].raw_matrix = FeatureState::Present([1., 0., 0., 1., 100., 0.]),
            19 => m.objects[0].path.as_mut().unwrap().paint.active = FeatureState::Present(true),
            20 => {
                m.capabilities
                    .insert("native_layout_graphics".into(), FeatureState::Absent);
            }
            _ => m.objects[1].image.as_mut().unwrap().page_envelope.left += 1.,
        }
        assert!(validate(&m).is_err(), "forgery {case}");
    }
    let mut value = serde_json::to_value(&base)?;
    value["objects"][0]["invented"] = serde_json::json!(true);
    assert!(
        validate_native_graphics_metadata(
            &serde_json::to_vec(&value)?,
            &summary(),
            &"4".repeat(64),
            1,
            &ResourceBudget::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn native_graphics_typed_allocation_upper_covers_shape_growth_and_rejects_unsupported_arrays()
-> Result<()> {
    let mut node = path(1);
    let small = envelope_metadata(run(&[node.clone()], &[0])?);
    let small_bytes = serde_json::to_vec(&small)?;
    let small_upper = native_metadata_working_upper(&small_bytes)?;
    node.segments = Some(
        std::iter::once(seg(NativeSegmentKind::MoveTo, [0., 0.], false))
            .chain((1..100).map(|i| seg(NativeSegmentKind::LineTo, [i as f32, 0.], false)))
            .collect(),
    );
    node.value.segments = Some(100);
    let large = envelope_metadata(run(&[node], &[0])?);
    let large_bytes = serde_json::to_vec(&large)?;
    assert!(native_metadata_working_upper(&large_bytes)? > small_upper);
    assert!(small_upper > small_bytes.len() as u64);
    let mut budget = ResourceBudget::default();
    budget.owned_working_bytes = small_upper - 1;
    assert!(
        validate_native_graphics_metadata(&small_bytes, &summary(), &"4".repeat(64), 1, &budget)
            .is_err()
    );
    let mut unsupported = serde_json::to_value(&small)?;
    unsupported["page"]["struct_nodes"] = serde_json::json!([{}]);
    assert!(native_metadata_working_upper(&serde_json::to_vec(&unsupported)?).is_err());
    Ok(())
}

#[tokio::test]
async fn native_graphics_actual_pdfium_nested_crop_rotation_userunit_and_image_placement()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("new-authored-geometry.pdf");
    std::fs::write(
        &source,
        include_bytes!("../../../tests/fixtures/native-graphics.pdf"),
    )?;
    let budget = ResourceBudget::default();
    let prepared = crate::prepare_document(
        &source,
        &dir.path().join("staging"),
        &crate::PreparationOptions,
        &budget,
    )
    .await?;
    let native = crate::extract_native(
        &prepared,
        &crate::NativeOptions {
            render_pages: false,
            page_window: 1,
            ..Default::default()
        },
        &budget,
    )
    .await?;
    let page = &native.pages()[0];
    let bytes = std::fs::read(
        prepared
            .staging_root()
            .join(&page.metadata_artifact.relative_path),
    )?;
    let metadata = validate_native_graphics_metadata(
        &bytes,
        prepared.summary(),
        native.native_profile_sha256(),
        1,
        &budget,
    )?;
    assert_eq!(metadata.geometry.rotation_quarter_turns, 1);
    assert_eq!(metadata.geometry.user_unit, 2.);
    let outside = |x: f32, y: f32, width: f32, height: f32| {
        x < -1.0
            || y < -1.0
            || x + width > metadata.page.page_width + 1.0
            || y + height > metadata.page.page_height + 1.0
    };
    assert!(metadata.native_text_metadata.characters.iter().any(|c|
        matches!(&c.geometry, crate::NativeGeometry::Positioned(b) if outside(b.x, b.y, b.width, b.height))));
    assert!(metadata.page.text_items.iter().any(|item| outside(
        item.x,
        item.y,
        item.width,
        item.height
    )));
    assert!(
        metadata
            .native_text_metadata
            .characters
            .iter()
            .filter_map(|c| char::from_u32(c.unicode))
            .collect::<String>()
            .contains("Native geometry alphabet")
    );
    assert_eq!(
        (
            metadata.geometry.width_points,
            metadata.geometry.height_points
        ),
        (240.0, 300.0)
    );
    assert_eq!(
        metadata.geometry.source_to_page,
        AffineTransform {
            a: 0.0,
            b: 2.0,
            c: 2.0,
            d: 0.0,
            e: -40.0,
            f: -20.0,
        }
    );
    assert_eq!(metadata.graphics.top_level_count, 3);
    assert_eq!(
        metadata.objects.iter().map(|o| o.kind).collect::<Vec<_>>(),
        vec![
            NativeObjectKind::Text,
            NativeObjectKind::Form,
            NativeObjectKind::Form,
            NativeObjectKind::Path,
            NativeObjectKind::Path,
            NativeObjectKind::Image,
            NativeObjectKind::Image
        ]
    );
    let top = metadata.objects.last().unwrap();
    assert_eq!(top.ordinal_path, vec![2]);
    let FeatureState::Present(raw) = top.raw_matrix else {
        panic!("matrix")
    };
    assert_eq!(raw, [24., 4., -8., 16., 90., 40.]);
    let img = top.image.as_ref().unwrap();
    assert_eq!((img.pixel_width, img.pixel_height), (2, 2));
    // Independent constants from Crop[10,20,160,140], Rotate90, UserUnit2:
    // V(x,y) = (2*(y-20), 2*(x-10)), not the stored transform under test.
    let expected = [[40., 160.], [48., 208.], [80., 192.], [72., 144.]];
    assert_eq!(img.page_quad, expected);
    assert_eq!(top.native_bounds, [82., 40., 114., 60.]);
    assert_eq!(
        top.page_bounds_quad,
        [[40., 144.], [40., 208.], [80., 208.], [80., 144.]]
    );
    assert_ne!(img.page_quad, top.page_bounds_quad);
    // Authored nested matrices, independently evaluated before the viewport:
    // page cm * outer Form Matrix * inner cm * inner Form Matrix * path point.
    assert_eq!(
        metadata.objects[3].path.as_ref().unwrap().segments[0].page_point,
        [98., 137.]
    );
    let nested_expected = [
        [140.25, 92.5],
        [182.75, 87.5],
        [188.75, 27.5],
        [146.25, 32.5],
    ];
    assert_eq!(
        metadata.objects[5].image.as_ref().unwrap().page_quad,
        nested_expected
    );
    assert!(matches!(
        img.pixel_orientation,
        FeatureState::Unsupported(_)
    ));
    assert_eq!(
        metadata.objects[3]
            .path
            .as_ref()
            .unwrap()
            .paint
            .raw_fill_rule,
        1
    );
    assert!(
        metadata.objects[4]
            .path
            .as_ref()
            .unwrap()
            .segments
            .iter()
            .any(|s| s.kind == NativeSegmentKind::BezierTo)
    );
    assert_eq!(page.coverage, crate::CoverageState::RequiredOcr);
    assert!(matches!(
        metadata.graphics.content_bounds,
        FeatureState::Present(_)
    ));
    for field in ["user_unit", "rotation_quarter_turns"] {
        let mut forged = metadata.clone();
        if field == "user_unit" {
            forged.geometry.user_unit = 3.0;
        } else {
            forged.geometry.rotation_quarter_turns = 2;
        }
        let error = validate_native_graphics_metadata(
            &serde_json::to_vec(&forged)?,
            prepared.summary(),
            native.native_profile_sha256(),
            1,
            &budget,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("native PDF viewport"),
            "{field}: {error}"
        );
    }
    Ok(())
}
