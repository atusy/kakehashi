use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use kakehashi::tree_worker::{
    Client, NavigateNode, NodeNavigation, RequestContext, ResolveNode, Response, SyncDocument,
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
    #[arg(long, default_value_t = 10_000)]
    requests: usize,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value_t = 200)]
    lines: usize,
}

fn context(document: usize, request_id: u64) -> RequestContext {
    RequestContext {
        request_id,
        worker_generation: 1,
        uri: format!("file:///stage6-node-benchmark-{document}.rs"),
        incarnation: 1,
        content_version: 1,
        configuration_generation: 0,
    }
}

fn sync_request(document: usize, parser: &Path, digest: &str, text: &str) -> SyncDocument {
    SyncDocument {
        context: context(document, document as u64 + 1),
        language: "rust".into(),
        grammar_symbol: "rust".into(),
        source_path: parser.to_path_buf(),
        parser_path: parser.to_path_buf(),
        artifact_digest: digest.into(),
        text: text.into(),
    }
}

fn elapsed_ns(started: Instant) -> u64 {
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

fn parse_rust(text: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    parser.parse(text, None).unwrap()
}

fn direct_round(tree: &tree_sitter::Tree, byte: usize) -> u64 {
    let started = Instant::now();
    let node = tree
        .root_node()
        .descendant_for_byte_range(byte, byte)
        .unwrap();
    black_box(node.parent().unwrap().kind());
    elapsed_ns(started)
}

fn worker_resolve(worker: &Client, document: usize, request_id: u64, byte: usize) -> (u64, String) {
    let started = Instant::now();
    let response = worker
        .resolve_node(ResolveNode {
            context: context(document, request_id),
            byte_offset: byte,
            named: false,
        })
        .unwrap();
    let Response::Nodes(result) = response else {
        panic!("node resolve failed: {response:?}");
    };
    let node = result.nodes.into_iter().next().expect("resolved node");
    black_box(&node.kind);
    (elapsed_ns(started), node.id.local_id)
}

fn worker_resolve_and_parent(
    worker: &Client,
    document: usize,
    request_id: u64,
    byte: usize,
) -> u64 {
    let started = Instant::now();
    let (_, local_id) = worker_resolve(worker, document, request_id, byte);
    let response = worker
        .navigate_node(NavigateNode {
            context: context(document, request_id + 1),
            node_id: kakehashi::tree_worker::OpaqueNodeId {
                worker_generation: 1,
                local_id,
            },
            operation: NodeNavigation::Parent,
        })
        .unwrap();
    let Response::Nodes(result) = response else {
        panic!("node navigation failed: {response:?}");
    };
    black_box(result.nodes.first().expect("parent node").kind.as_str());
    elapsed_ns(started)
}

fn concurrent_worker(
    worker: Arc<Client>,
    documents: usize,
    requests: usize,
    byte: usize,
) -> (Vec<u64>, f64) {
    let per_document = requests / documents;
    let started = Instant::now();
    let samples = std::thread::scope(|scope| {
        let handles = (0..documents)
            .map(|document| {
                let worker = Arc::clone(&worker);
                scope.spawn(move || {
                    (0..per_document)
                        .map(|index| {
                            worker_resolve(
                                &worker,
                                document + 1,
                                3_000_000 + (document * per_document + index) as u64,
                                byte,
                            )
                            .0
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let throughput = requests as f64 / started.elapsed().as_secs_f64();
    (samples, throughput)
}

fn main() {
    let args = Args::parse();
    assert!(args.requests > 0, "--requests must be positive");
    assert!(args.threads > 0, "--threads must be positive");
    assert_eq!(
        args.requests % args.threads,
        0,
        "--requests must be divisible by --threads"
    );

    let mut text = String::new();
    for index in 0..args.lines {
        text.push_str(&format!(
            "fn function_{index}() {{ let value_{index} = {index}; }}\n"
        ));
    }
    let marker = text.find("value_0").unwrap();
    let direct_tree = parse_rust(&text);
    let digest = kakehashi::tree_worker::artifact_digest(&args.parser).unwrap();
    let worker = Arc::new(Client::spawn(&args.bin, args.threads, 1).unwrap());
    for document in 0..=args.threads {
        let response = worker
            .sync_document(sync_request(document, &args.parser, &digest, &text))
            .unwrap();
        assert!(matches!(response, Response::DocumentAck(_)));
    }

    let mut direct = Vec::with_capacity(args.requests);
    let mut worker_resolve_samples = Vec::with_capacity(args.requests);
    let mut worker_navigation_samples = Vec::with_capacity(args.requests);
    for index in 0..args.requests {
        direct.push(direct_round(&direct_tree, marker));
        worker_resolve_samples.push(worker_resolve(&worker, 0, 1_000_000 + index as u64, marker).0);
        worker_navigation_samples.push(worker_resolve_and_parent(
            &worker,
            0,
            2_000_000 + index as u64 * 2,
            marker,
        ));
    }
    let (concurrent_samples, concurrent_throughput) =
        concurrent_worker(Arc::clone(&worker), args.threads, args.requests, marker);

    Arc::try_unwrap(worker)
        .ok()
        .expect("benchmark threads released worker")
        .shutdown()
        .unwrap();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "requests": args.requests,
            "threads": args.threads,
            "source_lines": args.lines,
            "source_bytes": text.len(),
            "direct_resolve_and_parent": summary(&direct),
            "worker_resolve_one_round_trip": summary(&worker_resolve_samples),
            "worker_resolve_and_parent_two_round_trips": summary(&worker_navigation_samples),
            "concurrent_documents_worker_resolve": {
                "documents": args.threads,
                "latency": summary(&concurrent_samples),
                "requests_per_second": concurrent_throughput,
            }
        }))
        .unwrap()
    );
}
