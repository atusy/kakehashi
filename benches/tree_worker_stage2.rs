use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use kakehashi::tree_worker::{
    ApplyDocumentEdits, ByteEdit, Client, DeriveDocumentSnapshot, LocalDocumentReplica, Request,
    RequestContext, Response, SyncDocument, encode_frame,
};
use serde_json::json;

#[derive(Parser)]
struct Args {
    #[arg(long = "bench", hide = true)]
    _bench: bool,
    #[arg(long)]
    bin: PathBuf,
    #[arg(long)]
    parser: PathBuf,
    #[arg(long, default_value_t = 1_000)]
    requests: usize,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value_t = 200)]
    lines: usize,
}

fn context(request_id: u64, version: u64) -> RequestContext {
    RequestContext {
        request_id,
        worker_generation: 1,
        uri: "file:///stage2-benchmark.rs".into(),
        incarnation: 1,
        content_version: version,
        configuration_generation: 0,
    }
}

fn sync_request(parser: &Path, text: &str) -> SyncDocument {
    SyncDocument {
        context: context(1, 1),
        language: "rust".into(),
        grammar_symbol: "rust".into(),
        parser_path: parser.to_path_buf(),
        text: text.into(),
    }
}

fn edit_request(request_id: u64, version: u64, marker: usize) -> ApplyDocumentEdits {
    ApplyDocumentEdits {
        context: context(request_id, version),
        base_version: version - 1,
        edits: vec![ByteEdit {
            start_byte: marker,
            old_end_byte: marker + 6,
            new_text: format!("{:06}", 100_000 + version % 800_000),
        }],
    }
}

fn expect_ack(response: Response) {
    match response {
        Response::DocumentAck(ack) if ack.incremental => {}
        response => panic!("incremental edit failed: {response:?}"),
    }
}

fn expect_snapshot(response: Response) {
    if !matches!(response, Response::Snapshot(_)) {
        panic!("document derive failed: {response:?}");
    }
}

fn direct_round(direct: &mut LocalDocumentReplica, version: u64, marker: usize) -> u64 {
    let started = Instant::now();
    expect_ack(direct.apply_document_edits(edit_request(version * 2, version, marker)));
    expect_snapshot(direct.derive_document_snapshot(DeriveDocumentSnapshot {
        context: context(version * 2 + 1, version),
    }));
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn worker_round(worker: &Client, version: u64, marker: usize) -> u64 {
    let started = Instant::now();
    expect_ack(
        worker
            .apply_document_edits(edit_request(version * 2, version, marker))
            .unwrap(),
    );
    expect_snapshot(
        worker
            .derive_document_snapshot(DeriveDocumentSnapshot {
                context: context(version * 2 + 1, version),
            })
            .unwrap(),
    );
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn percentile(samples: &[u64], percentile: usize) -> f64 {
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    samples[(samples.len() * percentile).div_ceil(100).saturating_sub(1)] as f64 / 1_000.0
}

fn summary(samples: &[u64]) -> serde_json::Value {
    json!({
        "p50_us": percentile(samples, 50),
        "p95_us": percentile(samples, 95),
        "p99_us": percentile(samples, 99),
        "mean_us": samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1_000.0,
    })
}

fn main() {
    let args = Args::parse();
    assert!(args.requests > 0, "--requests must be positive");
    assert!(args.threads > 0, "--threads must be positive");
    let mut text = "const COUNTER: usize = 100001;\n".to_string();
    let marker = text.find("100001").unwrap();
    text.extend(
        (0..args.lines).map(|index| format!("fn function_{index}() {{ let value = {index}; }}\n")),
    );

    let mut direct = LocalDocumentReplica::new();
    let direct_sync_started = Instant::now();
    let response = direct.sync_document(sync_request(&args.parser, &text));
    let direct_sync_us = direct_sync_started.elapsed().as_secs_f64() * 1_000_000.0;
    assert!(matches!(response, Response::DocumentAck(_)));

    let worker_started = Instant::now();
    let worker = Client::spawn(&args.bin, args.threads, 1).unwrap();
    let spawn_us = worker_started.elapsed().as_secs_f64() * 1_000_000.0;
    let worker_sync_started = Instant::now();
    let response = worker
        .sync_document(sync_request(&args.parser, &text))
        .unwrap();
    let worker_sync_us = worker_sync_started.elapsed().as_secs_f64() * 1_000_000.0;
    assert!(matches!(response, Response::DocumentAck(_)));

    let mut direct_samples = Vec::with_capacity(args.requests);
    let mut worker_samples = Vec::with_capacity(args.requests);
    for index in 0..args.requests {
        let version = index as u64 + 2;
        if index % 2 == 0 {
            direct_samples.push(direct_round(&mut direct, version, marker));
            worker_samples.push(worker_round(&worker, version, marker));
        } else {
            worker_samples.push(worker_round(&worker, version, marker));
            direct_samples.push(direct_round(&mut direct, version, marker));
        }
    }

    let mut apply_frame = Vec::new();
    encode_frame(
        &mut apply_frame,
        &Request::ApplyDocumentEdits(edit_request(2, 2, marker)),
    )
    .unwrap();
    let mut derive_frame = Vec::new();
    encode_frame(
        &mut derive_frame,
        &Request::DeriveDocumentSnapshot(DeriveDocumentSnapshot {
            context: context(3, 2),
        }),
    )
    .unwrap();
    worker.shutdown().unwrap();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "requests": args.requests,
            "threads": args.threads,
            "source_lines": args.lines,
            "source_bytes": text.len(),
            "apply_frame_bytes": apply_frame.len(),
            "derive_frame_bytes": derive_frame.len(),
            "spawn_us": spawn_us,
            "sync": {
                "direct_us": direct_sync_us,
                "worker_parent_observed_us": worker_sync_us,
            },
            "alternating_incremental_edit_and_derive": {
                "direct": summary(&direct_samples),
                "worker_parent_observed": summary(&worker_samples),
                "direct_requests_per_second": args.requests as f64
                    / (direct_samples.iter().sum::<u64>() as f64 / 1_000_000_000.0),
                "worker_requests_per_second": args.requests as f64
                    / (worker_samples.iter().sum::<u64>() as f64 / 1_000_000_000.0),
            }
        }))
        .unwrap()
    );
}
