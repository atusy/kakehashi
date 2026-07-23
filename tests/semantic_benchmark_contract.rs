#[path = "../benches/support/dirty_footprint.rs"]
mod dirty_footprint;
#[path = "../benches/support/semantic_baseline.rs"]
mod semantic_baseline;

use dirty_footprint::{gen_dirty_rust, gen_dirty_rust_block, gen_dirty_rust_replacements};
use semantic_baseline::{
    SemanticBaseline, TRACKED_MARKER, ValidationError, tracked_marker_line, validate_token_payload,
};
use serde_json::json;

fn initial_tokens() -> serde_json::Value {
    json!({
        "resultId": "baseline-1",
        "data": [
            0, 1, 3, 2, 0,
            1, 4, 2, 3, 0,
            0, 5, 1, 4, 0
        ]
    })
}

#[test]
fn locates_a_scenario_fixed_marker_independently_of_server_tokens() {
    let content = format!("// setup\n\n{TRACKED_MARKER}\nfn work() {{}}\n");
    assert_eq!(tracked_marker_line(&content), Ok(2));
}

#[test]
fn reconstructs_delta_and_validates_the_latest_typed_position() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.record_prefix_insert(2).unwrap();

    baseline
        .apply_response(&json!({
            "resultId": "baseline-2",
            "edits": [{
                "start": 5,
                "deleteCount": 5,
                "data": [1, 6, 2, 3, 0, 0, 5, 1, 4, 0]
            }]
        }))
        .unwrap();

    assert_eq!(baseline.result_id(), "baseline-2");
    assert_eq!(baseline.tracked_line(), 1);
    baseline
        .apply_response(&json!({
            "resultId": "baseline-3",
            "edits": [{"start": 15, "deleteCount": 5}]
        }))
        .unwrap();
}

#[test]
fn applies_multiple_delta_edits_against_the_original_token_array() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.record_prefix_insert(2).unwrap();

    baseline
        .apply_response(&json!({
            "resultId": "baseline-2",
            "edits": [
                {"start": 0, "deleteCount": 5},
                {
                    "start": 5,
                    "deleteCount": 5,
                    "data": [1, 6, 2, 3, 0]
                }
            ]
        }))
        .unwrap();

    assert_eq!(baseline.result_id(), "baseline-2");
}

#[test]
fn validates_a_position_after_deleting_a_typed_prefix() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.record_prefix_insert(2).unwrap();
    baseline.record_prefix_delete(1).unwrap();

    baseline
        .apply_response(&json!({
            "resultId": "baseline-2",
            "data": [
                0, 1, 3, 2, 0,
                1, 5, 2, 3, 0,
                0, 5, 1, 4, 0
            ]
        }))
        .unwrap();

    assert_eq!(baseline.result_id(), "baseline-2");
}

#[test]
fn rejects_a_semantically_stale_full_response() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.record_prefix_insert(1).unwrap();

    assert_eq!(
        baseline.apply_response(&initial_tokens()),
        Err(ValidationError::TrackedTokenMismatch {
            line: 1,
            expected_start: 5,
            actual_start: 4,
        })
    );
}

#[test]
fn rejects_a_stale_response_after_a_fixed_width_edit() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.expect_tracked_start(5);

    assert_eq!(
        baseline.apply_response(&initial_tokens()),
        Err(ValidationError::TrackedTokenMismatch {
            line: 1,
            expected_start: 5,
            actual_start: 4,
        })
    );
}

#[test]
fn rejects_out_of_bounds_delta_edits() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();

    assert_eq!(
        baseline.apply_response(&json!({
            "resultId": "baseline-2",
            "edits": [{"start": 14, "deleteCount": 2}]
        })),
        Err(ValidationError::EditOutOfBounds {
            edit_index: 0,
            start: 14,
            delete_count: 2,
            token_data_len: 15,
        })
    );
    assert_eq!(baseline.result_id(), "baseline-1");
    baseline
        .apply_response(&json!({
            "resultId": "baseline-3",
            "edits": [{"start": 10, "deleteCount": 5, "data": [0, 5, 1, 4, 0]}]
        }))
        .unwrap();
}

#[test]
fn rejects_overlapping_and_duplicate_start_delta_edits() {
    let cases = [
        json!([
            {"start": 0, "deleteCount": 10},
            {"start": 5, "deleteCount": 5}
        ]),
        json!([
            {"start": 5, "deleteCount": 0, "data": [0, 0, 0, 0, 0]},
            {"start": 5, "deleteCount": 5}
        ]),
    ];

    for edits in cases {
        let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
        assert_eq!(
            baseline.apply_response(&json!({
                "resultId": "baseline-2",
                "edits": edits
            })),
            Err(ValidationError::OverlappingEdits {
                first_edit_index: 0,
                second_edit_index: 1,
            })
        );
        assert_eq!(baseline.result_id(), "baseline-1");
    }
}

#[test]
fn validates_range_token_payload_shape() {
    assert_eq!(validate_token_payload(&json!({"data": []})), Ok(()));
    assert_eq!(
        validate_token_payload(&json!({"data": [0, 1]})),
        Err(ValidationError::InvalidTokenDataLength { len: 2 })
    );
    assert_eq!(
        validate_token_payload(&serde_json::Value::Null),
        Err(ValidationError::MissingTokenPayload)
    );
}

#[test]
fn dirty_rust_documents_define_exact_syntax_unit_footprints() {
    for dirty_units in [1, 10, 50] {
        let document = gen_dirty_rust(100, dirty_units);
        assert_eq!(
            document.dirty_units * 100 / document.total_units,
            dirty_units
        );
        assert_eq!(
            tracked_marker_line(&document.content),
            Ok(document.edit_start_line)
        );
        assert_eq!(
            document.edit_end_line - document.edit_start_line,
            u32::try_from(gen_dirty_rust_block(dirty_units, 0).lines().count()).unwrap()
        );
    }
}

#[test]
fn dirty_rust_replacements_are_unique_line_stable_and_freshness_visible() {
    let initial = gen_dirty_rust_block(10, 0);
    let next = gen_dirty_rust_block(10, 1);
    assert_ne!(initial, next);
    assert_eq!(initial.lines().count(), next.lines().count());
    assert!(initial.lines().next().unwrap().starts_with(TRACKED_MARKER));
    assert!(
        next.lines()
            .next()
            .unwrap()
            .starts_with(&format!(" {TRACKED_MARKER}"))
    );
}

#[test]
fn dirty_rust_replacements_can_be_precomputed_before_timing() {
    let replacements = gen_dirty_rust_replacements(10, 3);
    assert_eq!(replacements.len(), 3);
    for (index, replacement) in replacements.iter().enumerate() {
        assert!(replacement.starts_with(&" ".repeat(index + 1)));
    }
}
