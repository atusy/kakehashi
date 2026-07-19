use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use crate::language::coordinator::WorkerGrammarDescriptor;
use crate::lsp::text_sync::SequentialByteEdit;
use crate::tree_worker::{
    ApplyDocumentEditsAndDerive, ByteEdit, Client, CloseDocument, DeriveDocumentSnapshot,
    RequestContext, Response, SyncDocument,
};

const SHADOW_ENV: &str = "KAKEHASHI_TREE_WORKER_SHADOW";
const SHADOW_THREADS_ENV: &str = "KAKEHASHI_TREE_WORKER_THREADS";
const SHADOW_QUEUE_CAPACITY: usize = 256;
static NEXT_WORKER_GENERATION: AtomicU64 = AtomicU64::new(1);

enum ShadowCommand {
    Sync(SyncDocument),
    Apply {
        request: ApplyDocumentEditsAndDerive,
        fallback: SyncDocument,
    },
    Close(CloseDocument),
    Shutdown,
}

pub(super) struct TreeWorkerShadow {
    sender: Option<mpsc::SyncSender<ShadowCommand>>,
    disabled: Arc<AtomicBool>,
    next_request_id: AtomicU64,
    worker_generation: u64,
}

impl TreeWorkerShadow {
    pub(super) fn from_environment() -> Self {
        let disabled = Arc::new(AtomicBool::new(false));
        let worker_generation = NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed);
        if !shadow_enabled() {
            return Self {
                sender: None,
                disabled,
                next_request_id: AtomicU64::new(1),
                worker_generation,
            };
        }
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => {
                log::error!(target: "kakehashi::tree_worker_shadow", "cannot resolve worker executable: {error}");
                disabled.store(true, Ordering::Release);
                return Self {
                    sender: None,
                    disabled,
                    next_request_id: AtomicU64::new(1),
                    worker_generation,
                };
            }
        };
        let compute_threads = configured_compute_threads();
        let (sender, receiver) = mpsc::sync_channel(SHADOW_QUEUE_CAPACITY);
        let actor_disabled = Arc::clone(&disabled);
        std::thread::Builder::new()
            .name("kakehashi-tree-worker-shadow".into())
            .spawn(move || {
                run_actor(
                    receiver,
                    executable,
                    compute_threads,
                    worker_generation,
                    actor_disabled,
                );
            })
            .expect("tree worker shadow actor thread must spawn");
        log::info!(
            target: "kakehashi::tree_worker_shadow",
            "enabled one shadow worker with {compute_threads} compute threads"
        );
        Self {
            sender: Some(sender),
            disabled,
            next_request_id: AtomicU64::new(1),
            worker_generation,
        }
    }

    pub(super) fn mirror_full(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        language: String,
        grammar: WorkerGrammarDescriptor,
        text: String,
    ) {
        let request = self.sync_request(uri, incarnation, content_version, language, grammar, text);
        self.submit(ShadowCommand::Sync(request));
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.sender.is_some() && !self.disabled.load(Ordering::Acquire)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn mirror_change(
        &self,
        uri: &url::Url,
        incarnation: u64,
        base_version: u64,
        content_version: u64,
        language: String,
        grammar: WorkerGrammarDescriptor,
        text: String,
        edits: Option<Vec<SequentialByteEdit>>,
    ) {
        let fallback =
            self.sync_request(uri, incarnation, content_version, language, grammar, text);
        let Some(edits) = edits else {
            self.submit(ShadowCommand::Sync(fallback));
            return;
        };
        let request = ApplyDocumentEditsAndDerive {
            context: fallback.context.clone(),
            base_version,
            edits: edits
                .into_iter()
                .map(|edit| ByteEdit {
                    start_byte: edit.start_byte,
                    old_end_byte: edit.old_end_byte,
                    new_text: edit.new_text,
                })
                .collect(),
        };
        self.submit(ShadowCommand::Apply { request, fallback });
    }

    pub(super) fn mirror_close(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) {
        self.submit(ShadowCommand::Close(CloseDocument {
            context: self.context(uri, incarnation, content_version, configuration_generation),
        }));
    }

    pub(super) fn shutdown(&self) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(ShadowCommand::Shutdown);
        }
    }

    fn sync_request(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        language: String,
        grammar: WorkerGrammarDescriptor,
        text: String,
    ) -> SyncDocument {
        SyncDocument {
            context: self.context(
                uri,
                incarnation,
                content_version,
                grammar.configuration_generation,
            ),
            language,
            grammar_symbol: grammar.grammar_symbol,
            parser_path: grammar.parser_path,
            text,
        }
    }

    fn context(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> RequestContext {
        RequestContext {
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            worker_generation: self.worker_generation,
            uri: uri.as_str().to_string(),
            incarnation,
            content_version,
            configuration_generation,
        }
    }

    fn submit(&self, command: ShadowCommand) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        if let Err(error) = sender.try_send(command) {
            self.disabled.store(true, Ordering::Release);
            log::error!(
                target: "kakehashi::tree_worker_shadow",
                "disabled shadow worker after bounded queue failure: {error}"
            );
        }
    }
}

fn shadow_enabled() -> bool {
    std::env::var(SHADOW_ENV)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE"))
}

fn configured_compute_threads() -> usize {
    std::env::var(SHADOW_THREADS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|threads| threads.get().min(8))
                .unwrap_or(1)
        })
}

fn run_actor(
    receiver: mpsc::Receiver<ShadowCommand>,
    executable: PathBuf,
    compute_threads: usize,
    worker_generation: u64,
    disabled: Arc<AtomicBool>,
) {
    let client = match Client::spawn(&executable, compute_threads, worker_generation) {
        Ok(client) => client,
        Err(error) => {
            disabled.store(true, Ordering::Release);
            log::error!(target: "kakehashi::tree_worker_shadow", "worker spawn failed: {error}");
            return;
        }
    };
    while let Ok(command) = receiver.recv() {
        if disabled.load(Ordering::Acquire) || matches!(command, ShadowCommand::Shutdown) {
            break;
        }
        let started = Instant::now();
        let response = match command {
            ShadowCommand::Sync(request) => sync_and_derive(&client, request),
            ShadowCommand::Apply { request, fallback } => {
                match client.apply_document_edits_and_derive(request) {
                    Ok(Response::Snapshot(snapshot)) => Ok(Response::Snapshot(snapshot)),
                    Ok(Response::WorkerRestartRequired(required)) => {
                        Ok(Response::WorkerRestartRequired(required))
                    }
                    Ok(_) => sync_and_derive(&client, fallback),
                    Err(error) => Err(error),
                }
            }
            ShadowCommand::Close(request) => client.close_document(request),
            ShadowCommand::Shutdown => unreachable!(),
        };
        match response {
            Ok(Response::Snapshot(snapshot)) => log::debug!(
                target: "kakehashi::tree_worker_shadow_metrics",
                "uri={} version={} parent_us={} queue_us={} compute_us={}",
                snapshot.context.uri,
                snapshot.context.content_version,
                started.elapsed().as_micros(),
                snapshot.queue_wait_ns / 1_000,
                snapshot.compute_ns / 1_000,
            ),
            Ok(Response::WorkerRestartRequired(required)) => {
                disabled.store(true, Ordering::Release);
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "disabled shadow worker pending restart: {}",
                    required.reason
                );
                break;
            }
            Ok(Response::Error(error)) => log::warn!(
                target: "kakehashi::tree_worker_shadow",
                "shadow request rejected: {}",
                error.message
            ),
            Ok(_) => {}
            Err(error) => {
                disabled.store(true, Ordering::Release);
                log::error!(target: "kakehashi::tree_worker_shadow", "worker transport failed: {error}");
                break;
            }
        }
    }
    if let Err(error) = client.shutdown() {
        log::warn!(target: "kakehashi::tree_worker_shadow", "worker shutdown failed: {error}");
    }
}

fn sync_and_derive(client: &Client, request: SyncDocument) -> std::io::Result<Response> {
    let context = request.context.clone();
    match client.sync_document(request)? {
        Response::DocumentAck(_) => {
            client.derive_document_snapshot(DeriveDocumentSnapshot { context })
        }
        response => Ok(response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow(sender: mpsc::SyncSender<ShadowCommand>) -> TreeWorkerShadow {
        TreeWorkerShadow {
            sender: Some(sender),
            disabled: Arc::new(AtomicBool::new(false)),
            next_request_id: AtomicU64::new(1),
            worker_generation: 7,
        }
    }

    fn grammar() -> WorkerGrammarDescriptor {
        WorkerGrammarDescriptor {
            parser_path: "/parser/rust.so".into(),
            grammar_symbol: "rust".into(),
            configuration_generation: 3,
        }
    }

    #[test]
    fn mirror_change_preserves_sequential_edits_and_full_sync_fallback() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let uri = url::Url::parse("file:///example.rs").unwrap();

        shadow.mirror_change(
            &uri,
            2,
            4,
            5,
            "rust".into(),
            grammar(),
            "aXXbYde".into(),
            Some(vec![
                SequentialByteEdit {
                    start_byte: 1,
                    old_end_byte: 1,
                    new_text: "XX".into(),
                },
                SequentialByteEdit {
                    start_byte: 4,
                    old_end_byte: 5,
                    new_text: "Y".into(),
                },
            ]),
        );

        let ShadowCommand::Apply { request, fallback } = receiver.recv().unwrap() else {
            panic!("incremental mirror must enqueue an apply command");
        };
        assert_eq!(request.base_version, 4);
        assert_eq!(request.context.content_version, 5);
        assert_eq!(request.edits[0].new_text, "XX");
        assert_eq!(request.edits[1].start_byte, 4);
        assert_eq!(fallback.text, "aXXbYde");
    }

    #[test]
    fn bounded_queue_failure_disables_the_shadow_session() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        shadow.submit(ShadowCommand::Shutdown);
        shadow.submit(ShadowCommand::Shutdown);

        assert!(!shadow.is_enabled());
    }
}
