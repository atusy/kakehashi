//! Actor components for async bridge connection.
//!
//! This module provides the actor-based infrastructure for non-blocking
//! communication with downstream language servers (ADR-0015).
//!
//! # Components
//!
//! - `ResponseRouter`: Routes responses to pending requests via oneshot channels
//! - `Reader`: Background task that reads from server stdout and routes responses
//! - `Writer`: Background task that writes to server stdin from unified order queue

mod outbound_message;
mod reader;
mod response_router;
mod writer;

pub(crate) use outbound_message::OutboundMessage;

#[cfg(test)]
pub(crate) use reader::spawn_reader_task;
#[cfg(test)]
pub(crate) use reader::spawn_reader_task_with_liveness;
pub(crate) use reader::{ReaderTaskHandle, UpstreamNotification, spawn_reader_task_for_language};
#[cfg(test)]
pub(crate) use response_router::RouteResult;
pub(crate) use response_router::{ResponseRouter, RouterCleanupGuard};
pub(crate) use writer::{OUTBOUND_QUEUE_CAPACITY, WriterTaskHandle, spawn_writer_task};
