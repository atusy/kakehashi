use std::path::PathBuf;

use kakehashi::tree_worker::{Client, DeriveSnapshot, RequestContext, Response};

#[test]
fn real_worker_handshakes_and_contains_request_errors() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_kakehashi"));
    let mut worker = Client::spawn(&executable, 2, 41).unwrap();

    assert_eq!(worker.compute_threads(), 2);
    let response = worker
        .derive(DeriveSnapshot {
            context: RequestContext {
                request_id: 9,
                worker_generation: 41,
                uri: "file:///missing.rs".into(),
                incarnation: 1,
                content_version: 0,
                configuration_generation: 0,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: PathBuf::from("/missing/tree-sitter-rust"),
            text: "fn main() {}".into(),
        })
        .unwrap();

    let Response::Error(error) = response else {
        panic!("missing grammar must be a contained request error");
    };
    assert_eq!(error.request_id, Some(9));
    worker.shutdown().unwrap();
}
