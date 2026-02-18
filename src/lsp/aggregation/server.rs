mod dispatch;
mod fan_in;
mod fan_out;

pub(crate) use dispatch::{dispatch_collect_all, dispatch_first_win};
pub(crate) use fan_in::FanInResult;
