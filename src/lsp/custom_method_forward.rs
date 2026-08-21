//! Fallback dispatch for custom-method-host-forwarding.
//!
//! Sits directly around the `LspService`. A request the router answers with
//! `MethodNotFound` is re-issued as `kakehashi/forward/request`; a
//! notification kakehashi does not implement is issued as
//! `kakehashi/forward/notification`. Both carry `{ "method", "params" }` and
//! keep the original `id`, so the handler decides eligibility from
//! configuration and the client sees either the forwarded answer or the
//! router's original `MethodNotFound`.
//!
//! Built-in methods are never shadowed: requests are dispatched to the router
//! first and only its own "no handler" answer triggers the forward; the
//! notifications kakehashi implements are excluded by name
//! ([`HANDLED_NOTIFICATIONS`]).
//!
//! A gate predicate (`Kakehashi::custom_method_gate`) answers "could this
//! method be forwarded for some document under the current settings?" before
//! anything is cloned: the forward envelope needs the params, which the
//! router consumes, so without the gate every standard request would pay a
//! deep params clone for a forward that never happens.
//!
//! The inner service is shared behind a `std::sync::Mutex` because the
//! forward needs it again after the first answer arrives, inside the response
//! future, where `&mut self` is gone. The lock is only ever held
//! synchronously — around `poll_ready`/`call`, never across an await — so it
//! cannot deadlock, and the first dispatch of every message still happens
//! synchronously in [`Service::call`], in wire order, exactly as before.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tower::Service;
use tower_lsp_server::jsonrpc::{ErrorCode, Request, Response};

use crate::lsp::lsp_impl::HANDLED_NOTIFICATIONS;
use crate::lsp::lsp_impl::custom_method_forward::{
    FORWARD_NOTIFICATION_METHOD, FORWARD_REQUEST_METHOD,
};

/// Re-issues unhandled requests and notifications as the forwarding methods.
pub struct CustomMethodForwarder<S, G> {
    inner: Arc<Mutex<S>>,
    /// "Could this method be forwarded for some document?" — a superset
    /// filter; per-document eligibility is the handler's job.
    is_forwardable: G,
}

impl<S, G> CustomMethodForwarder<S, G> {
    pub fn new(inner: S, is_forwardable: G) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            is_forwardable,
        }
    }
}

/// What the forwarder does with one inbound message.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Hand to the router as-is: reserved namespaces, handled
    /// notifications, and the forwarding methods themselves.
    PassThrough,
    /// Dispatch as-is; if the router answers `MethodNotFound`, dispatch the
    /// rewritten request.
    ProbeThenForward,
    /// Dispatch the rewritten notification instead of the original (which
    /// the router would drop).
    ForwardNotification,
}

fn plan(req: &Request, is_forwardable: impl Fn(&str) -> bool) -> Plan {
    let method = req.method();
    // `$/` is the protocol's own namespace (cancel, progress, trace) and
    // `kakehashi/` is ours; neither is anyone's custom method. A method no
    // configuration names is not one either — and that check is what keeps
    // the standard methods off the clone-then-probe path.
    if method.starts_with("$/") || method.starts_with("kakehashi/") || !is_forwardable(method) {
        return Plan::PassThrough;
    }
    if req.id().is_some() {
        Plan::ProbeThenForward
    } else if HANDLED_NOTIFICATIONS.contains(&method) {
        Plan::PassThrough
    } else {
        Plan::ForwardNotification
    }
}

/// Wrap `req` into the forwarding envelope under `forward_method`, keeping
/// its id (if any).
fn rewrite(req: &Request, forward_method: &'static str) -> Request {
    let params = serde_json::json!({
        "method": req.method(),
        "params": req.params().cloned().unwrap_or(serde_json::Value::Null),
    });
    let builder = Request::build(forward_method).params(params);
    match req.id() {
        Some(id) => builder.id(id.clone()).finish(),
        None => builder.finish(),
    }
}

fn is_method_not_found(response: &Option<Response>) -> bool {
    response
        .as_ref()
        .and_then(Response::error)
        .is_some_and(|error| error.code == ErrorCode::MethodNotFound)
}

impl<S, G> Service<Request> for CustomMethodForwarder<S, G>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    G: Fn(&str) -> bool,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        lock(&self.inner).poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match plan(&req, &self.is_forwardable) {
            Plan::PassThrough => Box::pin(lock(&self.inner).call(req)),
            Plan::ForwardNotification => {
                let forwarded = rewrite(&req, FORWARD_NOTIFICATION_METHOD);
                Box::pin(lock(&self.inner).call(forwarded))
            }
            Plan::ProbeThenForward => {
                let forwarded = rewrite(&req, FORWARD_REQUEST_METHOD);
                let probe = lock(&self.inner).call(req);
                let inner = Arc::clone(&self.inner);
                Box::pin(async move {
                    let response = probe.await?;
                    if !is_method_not_found(&response) {
                        return Ok(response);
                    }
                    // No `poll_ready` here: the probe was admitted through the
                    // normal `poll_ready`, and the service cannot have gone
                    // back to initializing since, so calling directly is
                    // sound — and the inner `poll_ready` registers no waker
                    // while initializing, so awaiting it here could hang.
                    let forward = lock(&inner).call(forwarded);
                    forward.await
                })
            }
        }
    }
}

fn lock<S>(inner: &Mutex<S>) -> std::sync::MutexGuard<'_, S> {
    // The guard is never held across an await, so a poisoned lock can only
    // mean a panic inside `poll_ready`/`call` themselves; keep serving.
    inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tower_lsp_server::jsonrpc::{Error, Id};

    /// Inner service that answers `known/method` and records every call;
    /// anything else gets the router's `MethodNotFound` (requests) or
    /// silence (notifications).
    #[derive(Clone, Default)]
    struct Router {
        calls: Arc<StdMutex<Vec<Request>>>,
    }

    impl Service<Request> for Router {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request) -> Self::Future {
            self.calls.lock().unwrap().push(req.clone());
            let known = matches!(
                req.method(),
                "known/method" | FORWARD_REQUEST_METHOD | FORWARD_NOTIFICATION_METHOD
            );
            let (method, id, _) = req.into_parts();
            std::future::ready(Ok(id.map(|id| {
                if known {
                    Response::from_ok(id, serde_json::json!({ "answered": method }))
                } else {
                    let mut error = Error::method_not_found();
                    error.data = Some(serde_json::Value::String(method.into_owned()));
                    Response::from_error(id, error)
                }
            })))
        }
    }

    fn request(method: &'static str, id: i64) -> Request {
        Request::build(method)
            .id(id)
            .params(serde_json::json!({ "textDocument": { "uri": "file:///a.md" } }))
            .finish()
    }

    fn notification(method: &'static str) -> Request {
        Request::build(method)
            .params(serde_json::json!({ "n": 1 }))
            .finish()
    }

    /// The configured forward methods in these tests: anything under
    /// `custom/`, plus the handled/reserved names so the exclusion rules —
    /// not the gate — are what the pass-through assertions exercise.
    fn configured(method: &str) -> bool {
        method.starts_with("custom/") || !method.starts_with("known/")
    }

    async fn drive(router: &Router, req: Request) -> Option<Response> {
        let mut svc = CustomMethodForwarder::new(router.clone(), configured);
        std::future::poll_fn(|cx| svc.poll_ready(cx)).await.unwrap();
        svc.call(req).await.unwrap()
    }

    fn methods(router: &Router) -> Vec<String> {
        router
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.method().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn handled_request_is_answered_by_the_router_alone() {
        let router = Router::default();
        let response = drive(&router, request("known/method", 1)).await.unwrap();
        assert_eq!(response.result().unwrap()["answered"], "known/method");
        assert_eq!(methods(&router), ["known/method"]);
    }

    #[tokio::test]
    async fn unhandled_request_is_forwarded_with_its_id_and_params() {
        let router = Router::default();
        let response = drive(&router, request("custom/echo", 7)).await.unwrap();
        assert_eq!(response.id(), &Id::Number(7));
        assert_eq!(
            response.result().unwrap()["answered"],
            FORWARD_REQUEST_METHOD
        );
        assert_eq!(methods(&router), ["custom/echo", FORWARD_REQUEST_METHOD]);
        let forwarded = router.calls.lock().unwrap()[1].clone();
        assert_eq!(forwarded.id(), Some(&Id::Number(7)));
        assert_eq!(
            forwarded.params().unwrap(),
            &serde_json::json!({
                "method": "custom/echo",
                "params": { "textDocument": { "uri": "file:///a.md" } }
            })
        );
    }

    #[tokio::test]
    async fn unhandled_notification_is_forwarded_instead_of_dropped() {
        let router = Router::default();
        assert!(drive(&router, notification("custom/ping")).await.is_none());
        assert_eq!(methods(&router), [FORWARD_NOTIFICATION_METHOD]);
        let forwarded = router.calls.lock().unwrap()[0].clone();
        assert_eq!(forwarded.id(), None);
        assert_eq!(
            forwarded.params().unwrap(),
            &serde_json::json!({ "method": "custom/ping", "params": { "n": 1 } })
        );
    }

    #[tokio::test]
    async fn handled_notifications_and_reserved_namespaces_pass_through() {
        for method in [
            "textDocument/didOpen",
            "exit",
            "$/cancelRequest",
            "$/setTrace",
            "kakehashi/captures",
        ] {
            let router = Router::default();
            drive(&router, notification(method)).await;
            assert_eq!(
                methods(&router),
                [method],
                "{method} must reach the router as-is"
            );
        }
        // A `$/` request that the router does not know is NOT forwarded either.
        let router = Router::default();
        let response = drive(&router, request("$/unknown", 3)).await.unwrap();
        assert_eq!(response.error().unwrap().code, ErrorCode::MethodNotFound);
        assert_eq!(methods(&router), ["$/unknown"]);
    }

    #[tokio::test]
    async fn unconfigured_methods_pass_through_without_a_forward() {
        // The gate says no: the router's MethodNotFound stands and no second
        // dispatch happens — the standard-method hot path.
        let router = Router::default();
        let mut svc = CustomMethodForwarder::new(router.clone(), |_: &str| false);
        let response = svc.call(request("custom/echo", 1)).await.unwrap().unwrap();
        assert_eq!(response.error().unwrap().code, ErrorCode::MethodNotFound);
        assert_eq!(methods(&router), ["custom/echo"]);
        assert!(
            svc.call(notification("custom/ping"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(methods(&router), ["custom/echo", "custom/ping"]);
    }

    #[tokio::test]
    async fn forwarding_methods_called_directly_are_not_rewrapped() {
        let router = Router::default();
        let response = drive(&router, request(FORWARD_REQUEST_METHOD, 1))
            .await
            .unwrap();
        assert_eq!(
            response.result().unwrap()["answered"],
            FORWARD_REQUEST_METHOD
        );
        assert_eq!(methods(&router), [FORWARD_REQUEST_METHOD]);
    }

    #[tokio::test]
    async fn non_method_not_found_errors_are_returned_verbatim() {
        /// Answers every request with `ServerNotInitialized` (-32002).
        #[derive(Clone)]
        struct Uninitialized;
        impl Service<Request> for Uninitialized {
            type Response = Option<Response>;
            type Error = std::convert::Infallible;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, req: Request) -> Self::Future {
                let (_, id, _) = req.into_parts();
                std::future::ready(Ok(id.map(|id| {
                    Response::from_error(id, Error::new(ErrorCode::ServerError(-32002)))
                })))
            }
        }
        let mut svc = CustomMethodForwarder::new(Uninitialized, configured);
        let response = svc.call(request("custom/echo", 1)).await.unwrap().unwrap();
        assert_eq!(
            response.error().unwrap().code,
            ErrorCode::ServerError(-32002)
        );
    }
}
