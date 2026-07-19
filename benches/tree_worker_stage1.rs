use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use kakehashi::tree_worker::{
    Client, DeriveSnapshot, DerivedSnapshot, LocalDeriver, RequestContext, Response,
};
use rayon::prelude::*;
use serde_json::json;

thread_local! {
    static PARALLEL_LOCAL_DERIVER: std::cell::RefCell<LocalDeriver> =
        std::cell::RefCell::new(LocalDeriver::new());
}

#[derive(Parser)]
struct Args {
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

fn request(id: u64, generation: u64, parser: &std::path::Path, text: &str) -> DeriveSnapshot {
    DeriveSnapshot {
        context: RequestContext {
            request_id: id,
            worker_generation: generation,
            uri: format!("file:///benchmark-{}.rs", id % 16),
            incarnation: 1,
            content_version: id,
            configuration_generation: 0,
        },
        language: "rust".into(),
        grammar_symbol: "rust".into(),
        parser_path: parser.to_path_buf(),
        text: text.into(),
    }
}

fn snapshot(response: Response) -> DerivedSnapshot {
    match response {
        Response::Snapshot(snapshot) => snapshot,
        response => panic!("benchmark derive failed: {response:?}"),
    }
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let mut values = values.to_vec();
    values.sort_unstable();
    values[(values.len() * percentile).div_ceil(100).saturating_sub(1)]
}

fn summary(samples: &[u64]) -> serde_json::Value {
    json!({
        "p50_us": percentile(samples, 50) as f64 / 1_000.0,
        "p95_us": percentile(samples, 95) as f64 / 1_000.0,
        "p99_us": percentile(samples, 99) as f64 / 1_000.0,
        "mean_us": samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1_000.0,
    })
}

fn main() {
    let args = Args::parse();
    assert!(args.requests > 0, "--requests must be positive");
    assert!(args.threads > 0, "--threads must be positive");
    let text = (0..args.lines)
        .map(|index| format!("fn function_{index}() {{ let value = {index}; }}\n"))
        .collect::<String>();
    let generation = 1;

    let cold_started = Instant::now();
    let worker = Client::spawn(&args.bin, args.threads, generation).unwrap();
    let cold_start_us = cold_started.elapsed().as_secs_f64() * 1_000_000.0;
    snapshot(
        worker
            .derive(request(1, generation, &args.parser, &text))
            .unwrap(),
    );

    let mut local = LocalDeriver::new();
    snapshot(local.derive(request(2, generation, &args.parser, &text)));

    let mut direct_sequential = Vec::with_capacity(args.requests);
    for index in 0..args.requests {
        let started = Instant::now();
        snapshot(local.derive(request(
            10_000 + index as u64,
            generation,
            &args.parser,
            &text,
        )));
        direct_sequential.push(started.elapsed().as_nanos() as u64);
    }

    let mut worker_sequential = Vec::with_capacity(args.requests);
    let mut worker_compute = Vec::with_capacity(args.requests);
    let mut worker_queue = Vec::with_capacity(args.requests);
    for index in 0..args.requests {
        let started = Instant::now();
        let result = snapshot(
            worker
                .derive(request(
                    20_000 + index as u64,
                    generation,
                    &args.parser,
                    &text,
                ))
                .unwrap(),
        );
        worker_sequential.push(started.elapsed().as_nanos() as u64);
        worker_compute.push(result.compute_ns);
        worker_queue.push(result.queue_wait_ns);
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()
        .unwrap();
    let direct_parallel_started = Instant::now();
    let direct_parallel = pool.install(|| {
        (0..args.requests)
            .into_par_iter()
            .map(|index| {
                let started = Instant::now();
                let snapshot = PARALLEL_LOCAL_DERIVER.with(|local| {
                    snapshot(local.borrow_mut().derive(request(
                        30_000 + index as u64,
                        generation,
                        &args.parser,
                        &text,
                    )))
                });
                (
                    started.elapsed().as_nanos() as u64,
                    snapshot.parser_cache_hit,
                )
            })
            .collect::<Vec<_>>()
    });
    let direct_parallel_wall_ms = direct_parallel_started.elapsed().as_secs_f64() * 1_000.0;
    let (direct_parallel, direct_cache_hits): (Vec<_>, Vec<_>) =
        direct_parallel.into_iter().unzip();

    let worker = Arc::new(worker);
    let worker_parallel_started = Instant::now();
    let worker_parallel = pool.install(|| {
        (0..args.requests)
            .into_par_iter()
            .map(|index| {
                let started = Instant::now();
                let snapshot = snapshot(
                    worker
                        .derive(request(
                            40_000 + index as u64,
                            generation,
                            &args.parser,
                            &text,
                        ))
                        .unwrap(),
                );
                (
                    started.elapsed().as_nanos() as u64,
                    snapshot.parser_cache_hit,
                )
            })
            .collect::<Vec<_>>()
    });
    let worker_parallel_wall_ms = worker_parallel_started.elapsed().as_secs_f64() * 1_000.0;
    let (worker_parallel, worker_cache_hits): (Vec<_>, Vec<_>) =
        worker_parallel.into_iter().unzip();
    Arc::try_unwrap(worker)
        .ok()
        .expect("parallel benchmark released worker")
        .shutdown()
        .unwrap();

    let request_bytes = serde_json::to_vec(&kakehashi::tree_worker::Request::DeriveSnapshot(
        request(50_000, generation, &args.parser, &text),
    ))
    .unwrap()
    .len()
        + 4;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "requests": args.requests,
            "threads": args.threads,
            "source_lines": args.lines,
            "request_frame_bytes": request_bytes,
            "cold_start_us": cold_start_us,
            "sequential": {
                "direct": summary(&direct_sequential),
                "worker_parent_observed": summary(&worker_sequential),
                "worker_compute": summary(&worker_compute),
                "worker_queue": summary(&worker_queue),
            },
            "parallel": {
                "direct_request_latency": summary(&direct_parallel),
                "worker_request_latency": summary(&worker_parallel),
                "direct_wall_ms": direct_parallel_wall_ms,
                "worker_wall_ms": worker_parallel_wall_ms,
                "direct_parser_cache_hits": direct_cache_hits.into_iter().filter(|hit| *hit).count(),
                "worker_parser_cache_hits": worker_cache_hits.into_iter().filter(|hit| *hit).count(),
                "direct_requests_per_second": args.requests as f64 / (direct_parallel_wall_ms / 1_000.0),
                "worker_requests_per_second": args.requests as f64 / (worker_parallel_wall_ms / 1_000.0),
            },
        }))
        .unwrap()
    );
}
