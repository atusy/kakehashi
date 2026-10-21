mod concatenated_pipeline;
mod dispatch;
mod fan_in;
mod fan_out;
mod host_dispatch;
pub(crate) mod priority;

pub(crate) use concatenated_pipeline::run_sequential_format_pipeline;
pub(crate) use dispatch::{
    dispatch_concatenated, dispatch_preferred, dispatch_preferred_with_tokens,
    effective_priorities_from, mint_region_progress_source, preferred_among,
};
pub(crate) use fan_in::FanInResult;
#[cfg(test)]
pub(crate) use fan_in::preferred::preferred;
pub(crate) use fan_out::FanOutTask;
#[cfg(test)]
pub(crate) use fan_out::TaggedResult;
pub(crate) use host_dispatch::{
    HostFanOutTask, dispatch_host_concatenated, dispatch_host_preferred,
};
pub(crate) use priority::{expand_priorities, priorities_admit, truncate_entries};
