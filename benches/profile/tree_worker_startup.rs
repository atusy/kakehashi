//! Minimal harness for isolating tree-worker process startup and shutdown.

use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let mut threads = None;
    while let Some(argument) = arguments.next() {
        if argument == "--threads" {
            threads = arguments
                .next()
                .and_then(|value| value.to_string_lossy().parse::<usize>().ok());
        } else {
            panic!("unexpected argument: {}", PathBuf::from(argument).display());
        }
    }
    let threads = threads.expect("--threads is required");
    assert!(threads > 0, "--threads must be positive");
    let executable = std::env::current_exe()
        .expect("resolve harness executable")
        .parent()
        .and_then(|examples| examples.parent())
        .map(|release| release.join("kakehashi.exe"))
        .expect("resolve release directory");

    let handshake_started = Instant::now();
    let worker = kakehashi::tree_worker::Client::spawn(&executable, threads, 1)
        .expect("spawn tree worker");
    let handshake_us = handshake_started.elapsed().as_secs_f64() * 1_000_000.0;
    let shutdown_started = Instant::now();
    worker.shutdown().expect("shut down tree worker");
    let shutdown_us = shutdown_started.elapsed().as_secs_f64() * 1_000_000.0;

    println!(
        "{}",
        serde_json::json!({
            "handshake_us": handshake_us,
            "shutdown_us": shutdown_us,
        })
    );
}
