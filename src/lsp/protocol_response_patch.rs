//! Repairs response shapes omitted or mistyped by the pinned protocol crate.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::Service;
use tower_lsp_server::jsonrpc::{Request, Response};

#[derive(Clone)]
pub struct ProtocolResponsePatch<S> {
    inner: S,
}

impl<S> ProtocolResponsePatch<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Service<Request> for ProtocolResponsePatch<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let patch_type_hierarchy = request.method() == "textDocument/prepareTypeHierarchy";
        let future = self.inner.call(request);
        Box::pin(async move {
            let response = future.await?;
            if !patch_type_hierarchy {
                return Ok(response);
            }
            Ok(response.map(array_wrap_type_hierarchy_tags))
        })
    }
}

fn array_wrap_type_hierarchy_tags(response: Response) -> Response {
    let (id, body) = response.into_parts();
    let body = body.map(|mut result| {
        if let Some(items) = result.as_array_mut() {
            for item in items {
                if let Some(tags) = item.get_mut("tags")
                    && !tags.is_array()
                    && !tags.is_null()
                {
                    *tags = serde_json::Value::Array(vec![tags.take()]);
                }
            }
        }
        result
    });
    Response::from_parts(id, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    #[derive(Clone)]
    struct TaggedHierarchyService;

    impl Service<Request> for TaggedHierarchyService {
        type Response = Option<Response>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: Request) -> Self::Future {
            std::future::ready(Ok(request
                .id()
                .cloned()
                .map(|id| Response::from_ok(id, serde_json::json!([{ "tags": 1 }])))))
        }
    }

    #[tokio::test]
    async fn type_hierarchy_tags_are_serialized_as_protocol_arrays() {
        let mut service = ProtocolResponsePatch::new(TaggedHierarchyService);
        let response = service
            .call(
                Request::build("textDocument/prepareTypeHierarchy")
                    .id(1)
                    .finish(),
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            response.result().unwrap()[0]["tags"],
            serde_json::json!([1])
        );
    }
}
