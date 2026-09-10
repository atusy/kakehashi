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
        let patch_type_hierarchy = matches!(
            request.method(),
            "textDocument/prepareTypeHierarchy"
                | "typeHierarchy/supertypes"
                | "typeHierarchy/subtypes"
        );
        let advertise_type_hierarchy = request.method() == "initialize";
        let request = scalarize_type_hierarchy_request_tags(request);
        let future = self.inner.call(request);
        Box::pin(async move {
            let response = future.await?;
            let mut response = response;
            if patch_type_hierarchy {
                response = response.map(array_wrap_type_hierarchy_tags);
            }
            if advertise_type_hierarchy {
                response = response.map(advertise_type_hierarchy_provider);
            }
            Ok(response)
        })
    }
}

fn scalarize_type_hierarchy_request_tags(request: Request) -> Request {
    if !matches!(
        request.method(),
        "typeHierarchy/supertypes" | "typeHierarchy/subtypes"
    ) {
        return request;
    }
    let (method, id, mut params) = request.into_parts();
    if let Some(tags) = params
        .as_mut()
        .and_then(|params| params.pointer_mut("/item/tags"))
        && let Some(values) = tags.as_array_mut()
    {
        *tags = values.first().cloned().unwrap_or(serde_json::Value::Null);
    }
    let mut builder = Request::build(method);
    if let Some(params) = params {
        builder = builder.params(params);
    }
    if let Some(id) = id {
        builder = builder.id(id);
    }
    builder.finish()
}

fn advertise_type_hierarchy_provider(response: Response) -> Response {
    let (id, body) = response.into_parts();
    let body = body.map(|mut result| {
        if let Some(capabilities) = result
            .get_mut("capabilities")
            .and_then(serde_json::Value::as_object_mut)
        {
            capabilities.insert(
                "typeHierarchyProvider".into(),
                serde_json::Value::Bool(true),
            );
        }
        result
    });
    Response::from_parts(id, body)
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

    #[test]
    fn type_hierarchy_request_tags_are_scalarized_for_the_pinned_model() {
        let request = Request::build("typeHierarchy/supertypes")
            .id(1)
            .params(serde_json::json!({ "item": { "tags": [1] } }))
            .finish();

        let request = scalarize_type_hierarchy_request_tags(request);

        assert_eq!(request.params().unwrap()["item"]["tags"], 1);
    }

    #[test]
    fn subtype_request_tags_are_scalarized_for_the_pinned_model() {
        let request = Request::build("typeHierarchy/subtypes")
            .id(1)
            .params(serde_json::json!({ "item": { "tags": [1] } }))
            .finish();

        let request = scalarize_type_hierarchy_request_tags(request);

        assert_eq!(request.params().unwrap()["item"]["tags"], 1);
    }

    #[test]
    fn initialize_advertises_the_completed_type_hierarchy_surface() {
        let response = Response::from_ok(
            1.into(),
            serde_json::json!({ "capabilities": { "hoverProvider": true } }),
        );

        let response = advertise_type_hierarchy_provider(response);

        assert_eq!(
            response.result().unwrap()["capabilities"]["typeHierarchyProvider"],
            true
        );
        assert_eq!(
            response.result().unwrap()["capabilities"]["hoverProvider"],
            true
        );
    }
}
