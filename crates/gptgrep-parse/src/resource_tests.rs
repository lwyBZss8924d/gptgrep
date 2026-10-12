use super::*;

fn configured() -> ResourceBudget {
    ResourceBudget {
        image_file_bytes: 4 * 1024 * 1024,
        image_base64_bytes: 5_592_408,
        image_file_bytes_per_page: 4 * 1024 * 1024,
        ..ResourceBudget::default()
    }
}

#[test]
fn configured_image_capacity_exceeds_default_with_coupled_wire_admission() {
    let budget = configured();
    budget.validate().unwrap();
    budget.admit_image(2000, 2000, 4 * 1024 * 1024).unwrap();
    assert!(
        ResourceBudget::default()
            .admit_image(2000, 2000, 4 * 1024 * 1024)
            .is_err()
    );
    let native = budget.native_image_limits().unwrap();
    assert_eq!(native.image_file_bytes, 4 * 1024 * 1024);
    assert_eq!(native.image_base64_bytes, 5_592_408);
    assert!(native.image_wire_bytes > 4 * 1024 * 1024);
    assert!(native.image_transport_bytes >= native.image_wire_bytes);
    assert!(native.transport_bytes().unwrap() <= budget.owned_working_bytes);
    assert!(budget.admit_image(2000, 2000, 4 * 1024 * 1024 + 1).is_err());
    assert!(budget.admit_image(4000, 4000, 1).is_err());
}

#[test]
fn incoherent_and_overflowing_image_budgets_fail_before_allocation() {
    for mutation in 0..6 {
        let mut budget = configured();
        match mutation {
            0 => budget.image_file_bytes = 0,
            1 => budget.image_base64_bytes -= 1,
            2 => budget.image_file_bytes_per_page -= 1,
            3 => budget.image_file_bytes = u64::MAX,
            4 => budget.image_file_bytes_per_page = u64::MAX,
            _ => budget.image_base64_bytes = u64::MAX,
        }
        assert!(budget.validate().is_err(), "mutation {mutation}");
    }
}

#[test]
fn larger_configuration_cannot_raise_original_server_admission() {
    let admitted = ResourceBudget::default();
    let mut selected = configured();
    assert!(selected.adopt_shared_staging(&admitted).is_err());
    let admitted = configured();
    let mut selected = ResourceBudget::default();
    selected.adopt_shared_staging(&admitted).unwrap();
    selected.reserve_retained_staging(100).unwrap();
    assert_eq!(admitted.staging_used(), 100);
    selected.ensure_same_invocation_within(&admitted).unwrap();
    selected.image_file_bytes_per_page += 1;
    assert!(selected.ensure_same_invocation_within(&admitted).is_err());
}

#[test]
fn configured_corpus_staging_exceeds_defaults_but_keeps_atomic_shared_admission() {
    let admitted = ResourceBudget {
        document_staging_bytes: 2 * 1024 * 1024 * 1024,
        run_staging_bytes: 8 * 1024 * 1024 * 1024,
        owned_working_bytes: 1024 * 1024 * 1024,
        ..ResourceBudget::default()
    };
    admitted.validate().unwrap();
    let mut selected = admitted.clone();
    selected.adopt_shared_staging(&admitted).unwrap();
    selected
        .reserve_retained_staging(3 * 1024 * 1024 * 1024)
        .unwrap();
    assert_eq!(admitted.staging_used(), 3 * 1024 * 1024 * 1024);
    assert!(
        selected
            .reserve_retained_staging(6 * 1024 * 1024 * 1024)
            .is_err()
    );
    assert_eq!(admitted.staging_used(), 3 * 1024 * 1024 * 1024);
    selected.run_staging_bytes += 1;
    assert!(selected.ensure_same_invocation_within(&admitted).is_err());
    assert!(
        selected
            .adopt_shared_staging(&ResourceBudget::default())
            .is_err()
    );
}
