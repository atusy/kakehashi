#[path = "../benches/support/semantic_baseline.rs"]
mod semantic_baseline;

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
fn rejects_a_stale_empty_delta_even_with_a_new_result_id() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.record_prefix_insert(1).unwrap();
    assert_eq!(
        baseline.apply_response(&json!({"resultId": "fresh-id", "edits": []})),
        Err(ValidationError::TrackedTokenMismatch {
            line: 1,
            expected_start: 5,
            actual_start: 4,
        })
    );
    assert_eq!(baseline.result_id(), "baseline-1");
    baseline.record_prefix_delete(1).unwrap();
    baseline
        .apply_response(&json!({"resultId": "unchanged", "edits": []}))
        .unwrap();
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

#[path = "../benches/support/semantic_fixture.rs"]
mod semantic_fixture;

#[test]
fn sparse_edit_states_remain_valid_rust_at_exact_sizes() {
    use semantic_fixture::{
        FIXED_WIDTH_LINE_BYTES, FIXED_WIDTH_STATE_COUNT, fixed_width_marker_line, gen_sparse_rust,
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    for bytes in [32 * 1024 - 128, 32 * 1024, 32 * 1024 + 128, 64 * 1024] {
        let initial = gen_sparse_rust(bytes);
        assert!(
            !parser
                .parse(&initial, None)
                .unwrap()
                .root_node()
                .has_error()
        );
        let line_start = initial.find('\n').unwrap() + 1;
        let mut seen = std::collections::HashSet::new();
        for state in 0..FIXED_WIDTH_STATE_COUNT {
            let mut edited = initial.clone();
            edited.replace_range(
                line_start..line_start + FIXED_WIDTH_LINE_BYTES,
                &fixed_width_marker_line(state),
            );
            assert_eq!(edited.len(), bytes);
            assert!(seen.insert(edited.clone()));
            assert_eq!(
                edited.lines().nth(1).unwrap().find(TRACKED_MARKER),
                Some(state)
            );
            assert!(
                !parser.parse(&edited, None).unwrap().root_node().has_error(),
                "bytes={bytes}, state={state}"
            );
        }
    }
}
