use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kakehashi::tree_worker::{
    ApplyDocumentEdits, ByteEdit, Client, ConfigureLanguages, DeriveDocumentSnapshot,
    DeriveSemanticTokens, DocumentMemoryStats, InspectDocumentMemory, NodeLayerSelector,
    NodeScalarOperation, NodeScalarValue, RequestContext, ResolveNode, Response, RunNodeScalar,
    SyncDocument, WorkerLanguageAsset, WorkerLanguageCatalog, WorkerMemoryBudgets,
    WorkerQuerySources,
};
use serde::Serialize;

#[derive(Debug)]
struct Options {
    worker_bin: PathBuf,
    data_dir: PathBuf,
    derived_cache_soft_bytes: usize,
    non_evictable_estimate_hard_bytes: usize,
    hold_open: Duration,
}

#[derive(Serialize)]
struct StressReport {
    worker_generation: u64,
    derived_cache_soft_bytes: usize,
    non_evictable_estimate_hard_bytes: usize,
    first_a_us: u128,
    first_b_us: u128,
    recompute_a_us: u128,
    rejected_full_sync_us: u128,
    rejected_incremental_edit_us: u128,
    recovery_edit_us: u128,
    token_count: usize,
    recomputed_tokens_match: bool,
    node_identity_survived: bool,
    full_sync_rejection: String,
    incremental_edit_rejection: String,
    a_after_pressure: DocumentMemoryStats,
    b_after_pressure: DocumentMemoryStats,
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let parser = options
        .data_dir
        .join("parser")
        .join(format!("rust.{}", std::env::consts::DLL_EXTENSION));
    if !parser.is_file() {
        return Err(format!("installed Rust parser not found at {}", parser.display()).into());
    }
    let parser_digest = kakehashi::tree_worker::artifact_digest(&parser)?;
    let worker_generation = 1;
    let worker = Client::spawn_with_memory_budgets(
        &options.worker_bin,
        1,
        worker_generation,
        WorkerMemoryBudgets::new(
            options.derived_cache_soft_bytes,
            options.non_evictable_estimate_hard_bytes,
        )?,
    )?;

    configure_rust(&worker, &parser, &parser_digest, worker_generation)?;
    let a = context(2, worker_generation, "file:///memory-stress-a.rs", 1);
    let b = context(3, worker_generation, "file:///memory-stress-b.rs", 1);
    sync_rust(&worker, &parser, &parser_digest, a.clone(), "fn main() {}")?;
    sync_rust(&worker, &parser, &parser_digest, b.clone(), "fn main() {}")?;

    let node = match worker.resolve_node(ResolveNode {
        context: RequestContext {
            request_id: 4,
            ..a.clone()
        },
        byte_offset: 3,
        named: true,
        layer: NodeLayerSelector::Host,
    })? {
        Response::Nodes(nodes) => {
            nodes
                .nodes
                .into_iter()
                .next()
                .ok_or("worker returned no node for document A")?
                .id
        }
        response => return Err(format!("node lookup failed: {response:?}").into()),
    };

    let (a_first, first_a_us) = timed_semantic(&worker, 5, &a)?;
    let (b_first, first_b_us) = timed_semantic(&worker, 6, &b)?;
    if a_first.cache_hit || b_first.cache_hit {
        return Err("first semantic derivations unexpectedly hit a cache".into());
    }
    let a_after_pressure = inspect_memory(&worker, 7, &a)?;
    let b_after_pressure = inspect_memory(&worker, 8, &b)?;
    if a_after_pressure.result_cache_bytes != 0
        || a_after_pressure.auxiliary_cache_bytes != 0
        || b_after_pressure.result_cache_bytes == 0
        || b_after_pressure.auxiliary_cache_bytes != 0
    {
        return Err(format!(
            "soft-pressure order was not observable: A={a_after_pressure:?} B={b_after_pressure:?}"
        )
        .into());
    }

    let (a_recomputed, recompute_a_us) = timed_semantic(&worker, 9, &a)?;
    if a_recomputed.cache_hit {
        return Err("evicted document A unexpectedly hit its semantic cache".into());
    }
    let recomputed_tokens_match = a_recomputed.tokens == a_first.tokens;
    let node_identity_survived = matches!(
        worker.run_node_scalar(RunNodeScalar {
            context: RequestContext {
                request_id: 10,
                ..a.clone()
            },
            node_id: node,
            operation: NodeScalarOperation::Text,
        })?,
        Response::NodeScalar(result)
            if result.value == Some(NodeScalarValue::String("main".into()))
    );

    let oversized_body = (0..200)
        .map(|index| format!("let value_{index} = {index};"))
        .collect::<String>();
    let oversized_source = format!("fn main() {{ {oversized_body} }}");
    let full_started = Instant::now();
    let full_response = sync_rust_response(
        &worker,
        &parser,
        &parser_digest,
        context(11, worker_generation, &a.uri, 2),
        oversized_source,
    )?;
    let rejected_full_sync_us = full_started.elapsed().as_micros();
    let full_sync_rejection = contained_budget_error(full_response)?;
    assert_snapshot_version(&worker, 12, &a, "fn main() {}".len())?;

    let edit_started = Instant::now();
    let edit_response = worker.apply_document_edits(ApplyDocumentEdits {
        context: context(13, worker_generation, &a.uri, 2),
        base_version: 1,
        edits: vec![ByteEdit {
            start_byte: "fn main() {}".len() - 1,
            old_end_byte: "fn main() {}".len() - 1,
            new_text: oversized_body,
        }],
    })?;
    let rejected_incremental_edit_us = edit_started.elapsed().as_micros();
    let incremental_edit_rejection = contained_budget_error(edit_response)?;
    assert_snapshot_version(&worker, 14, &a, "fn main() {}".len())?;

    let recovery_started = Instant::now();
    let recovery = worker.apply_document_edits(ApplyDocumentEdits {
        context: context(15, worker_generation, &a.uri, 2),
        base_version: 1,
        edits: vec![ByteEdit {
            start_byte: 3,
            old_end_byte: 7,
            new_text: "next".into(),
        }],
    })?;
    let recovery_edit_us = recovery_started.elapsed().as_micros();
    if !matches!(recovery, Response::DocumentAck(_)) {
        return Err(format!("post-rejection recovery edit failed: {recovery:?}").into());
    }
    assert_snapshot_version(
        &worker,
        16,
        &context(16, worker_generation, &a.uri, 2),
        "fn next() {}".len(),
    )?;

    let report = StressReport {
        worker_generation,
        derived_cache_soft_bytes: options.derived_cache_soft_bytes,
        non_evictable_estimate_hard_bytes: options.non_evictable_estimate_hard_bytes,
        first_a_us,
        first_b_us,
        recompute_a_us,
        rejected_full_sync_us,
        rejected_incremental_edit_us,
        recovery_edit_us,
        token_count: a_first.tokens.len(),
        recomputed_tokens_match,
        node_identity_survived,
        full_sync_rejection,
        incremental_edit_rejection,
        a_after_pressure,
        b_after_pressure,
    };
    println!("{}", serde_json::to_string(&report)?);
    std::thread::sleep(options.hold_open);
    worker.shutdown()?;
    Ok(())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut worker_bin = None;
    let mut data_dir = None;
    let mut derived_cache_soft_bytes = 20;
    let mut non_evictable_estimate_hard_bytes = 4096;
    let mut hold_open = Duration::from_secs(1);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--bin" => worker_bin = Some(PathBuf::from(value)),
            "--data-dir" => data_dir = Some(PathBuf::from(value)),
            "--derived-cache-soft-bytes" => derived_cache_soft_bytes = value.parse()?,
            "--non-evictable-estimate-hard-bytes" => {
                non_evictable_estimate_hard_bytes = value.parse()?
            }
            "--hold-open" => hold_open = Duration::from_secs_f64(value.parse()?),
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    Ok(Options {
        worker_bin: worker_bin.ok_or("--bin is required")?,
        data_dir: data_dir.ok_or("--data-dir is required")?,
        derived_cache_soft_bytes,
        non_evictable_estimate_hard_bytes,
        hold_open,
    })
}

fn context(
    request_id: u64,
    worker_generation: u64,
    uri: &str,
    content_version: u64,
) -> RequestContext {
    RequestContext {
        request_id,
        worker_generation,
        uri: uri.into(),
        incarnation: 1,
        content_version,
        configuration_generation: 0,
    }
}

fn configure_rust(
    worker: &Client,
    parser: &Path,
    parser_digest: &str,
    worker_generation: u64,
) -> Result<(), Box<dyn Error>> {
    let response = worker.configure_languages(ConfigureLanguages {
        context: context(1, worker_generation, "kakehashi://memory-stress-catalog", 0),
        catalog: WorkerLanguageCatalog {
            assets: vec![WorkerLanguageAsset {
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: parser.into(),
                parser_path: parser.into(),
                artifact_digest: parser_digest.into(),
                queries: WorkerQuerySources::default(),
            }],
            capture_mappings: serde_json::from_value(serde_json::json!({
                "rust": { "highlights": { "variable": "keyword" } }
            }))?,
            search_paths: Vec::new(),
        },
    })?;
    if matches!(response, Response::LanguageCatalogAck(_)) {
        Ok(())
    } else {
        Err(format!("language catalog failed: {response:?}").into())
    }
}

fn sync_rust(
    worker: &Client,
    parser: &Path,
    parser_digest: &str,
    context: RequestContext,
    text: &str,
) -> Result<(), Box<dyn Error>> {
    let response = sync_rust_response(worker, parser, parser_digest, context, text.into())?;
    if matches!(response, Response::DocumentAck(_)) {
        Ok(())
    } else {
        Err(format!("document sync failed: {response:?}").into())
    }
}

fn sync_rust_response(
    worker: &Client,
    parser: &Path,
    parser_digest: &str,
    context: RequestContext,
    text: String,
) -> Result<Response, Box<dyn Error>> {
    Ok(worker.sync_document(SyncDocument {
        context,
        language: "rust".into(),
        grammar_symbol: "rust".into(),
        source_path: parser.into(),
        parser_path: parser.into(),
        artifact_digest: parser_digest.into(),
        queries: WorkerQuerySources {
            highlights: Some("(identifier) @variable".into()),
            ..Default::default()
        },
        text,
    })?)
}

fn timed_semantic(
    worker: &Client,
    request_id: u64,
    context: &RequestContext,
) -> Result<(kakehashi::tree_worker::DerivedSemanticTokens, u128), Box<dyn Error>> {
    let started = Instant::now();
    let response = worker.derive_semantic_tokens(DeriveSemanticTokens {
        context: RequestContext {
            request_id,
            ..context.clone()
        },
        supports_multiline: false,
    })?;
    let elapsed = started.elapsed().as_micros();
    match response {
        Response::SemanticTokens(result) => Ok((result, elapsed)),
        response => Err(format!("semantic derivation failed: {response:?}").into()),
    }
}

fn inspect_memory(
    worker: &Client,
    request_id: u64,
    context: &RequestContext,
) -> Result<DocumentMemoryStats, Box<dyn Error>> {
    match worker.inspect_document_memory(InspectDocumentMemory {
        context: RequestContext {
            request_id,
            ..context.clone()
        },
    })? {
        Response::DocumentMemory(result) => Ok(result),
        response => Err(format!("memory inspection failed: {response:?}").into()),
    }
}

fn contained_budget_error(response: Response) -> Result<String, Box<dyn Error>> {
    match response {
        Response::Error(error) if error.message.contains("non-evictable budget") => {
            Ok(error.message)
        }
        response => Err(format!("expected contained budget error, got {response:?}").into()),
    }
}

fn assert_snapshot_version(
    worker: &Client,
    request_id: u64,
    context: &RequestContext,
    expected_len: usize,
) -> Result<(), Box<dyn Error>> {
    match worker.derive_document_snapshot(DeriveDocumentSnapshot {
        context: RequestContext {
            request_id,
            ..context.clone()
        },
    })? {
        Response::Snapshot(snapshot) if snapshot.root_end_byte == expected_len => Ok(()),
        response => Err(format!("snapshot was not preserved: {response:?}").into()),
    }
}
