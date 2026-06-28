pub(crate) mod concatenated;
pub(crate) mod preferred;
pub(crate) mod result;

pub(crate) use result::FanInResult;

#[cfg(test)]
pub(super) mod test_helpers {
    use std::io;

    use tokio::task::JoinSet;

    use super::FanInResult;
    use crate::lsp::aggregation::server::fan_out::TaggedResult;

    pub(super) fn assert_done<T: std::fmt::Debug>(result: FanInResult<T>) -> T {
        match result {
            FanInResult::Done(v) => v,
            FanInResult::NoResult { errors } => {
                panic!("expected Done, got NoResult {{ errors: {errors} }}")
            }
            FanInResult::Cancelled => panic!("expected Done, got Cancelled"),
        }
    }

    pub(super) fn assert_no_result<T: std::fmt::Debug>(result: FanInResult<T>) -> usize {
        match result {
            FanInResult::NoResult { errors } => errors,
            FanInResult::Done(v) => panic!("expected NoResult, got Done({v:?})"),
            FanInResult::Cancelled => panic!("expected NoResult, got Cancelled"),
        }
    }

    pub(super) fn assert_cancelled<T: std::fmt::Debug>(result: FanInResult<T>) {
        match result {
            FanInResult::Cancelled => {}
            FanInResult::Done(v) => panic!("expected Cancelled, got Done({v:?})"),
            FanInResult::NoResult { .. } => panic!("expected Cancelled, got NoResult"),
        }
    }

    /// Spawn a TaggedResult task with a default server name.
    pub(super) fn spawn_tagged<T: Send + 'static>(
        join_set: &mut JoinSet<TaggedResult<T>>,
        value: io::Result<T>,
    ) {
        spawn_tagged_named(join_set, "test_server", value);
    }

    /// Spawn a TaggedResult task with a specific server name.
    pub(super) fn spawn_tagged_named<T: Send + 'static>(
        join_set: &mut JoinSet<TaggedResult<T>>,
        name: &str,
        value: io::Result<T>,
    ) {
        let name = name.to_string();
        join_set.spawn(async move {
            TaggedResult {
                server_name: name,
                value,
            }
        });
    }

    /// Spawn a task that panics, so `join_next` yields a `JoinError` — the
    /// shape of a downstream-request task that unwound instead of returning
    /// `Err` (the case the panic sink must surface, #506).
    pub(super) fn spawn_panicking<T: Send + 'static>(join_set: &mut JoinSet<TaggedResult<T>>) {
        async fn panicking_task<T>() -> TaggedResult<T> {
            panic!("simulated bridge task panic");
        }
        join_set.spawn(panicking_task::<T>());
    }
}
