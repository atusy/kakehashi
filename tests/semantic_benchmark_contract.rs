#[path = "../benches/support/semantic_baseline.rs"]
mod semantic_baseline;

use semantic_baseline::{SemanticBaseline, ValidationError};
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
fn reconstructs_delta_and_validates_the_latest_typed_position() {
    let mut baseline = SemanticBaseline::from_full(&initial_tokens(), 1).unwrap();
    baseline.record_prefix_insert(2).unwrap();

    baseline
        .apply_response(&json!({
            "resultId": "baseline-2",
            "edits": [{
                "start": 5,
                "deleteCount": 5,
                "data": [1, 6, 2, 3, 0]
            }]
        }))
        .unwrap();

    assert_eq!(baseline.result_id(), "baseline-2");
    assert_eq!(baseline.tracked_line(), 1);
    assert_eq!(baseline.data()[5..10], [1, 6, 2, 3, 0]);
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
    assert_eq!(
        baseline.data(),
        initial_tokens()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap() as u32)
            .collect::<Vec<_>>()
    );
}
