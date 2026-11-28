//! The JSON-RPC transport the pAMM feeds that read a chain read it through.

use alloy::{
    providers::RootProvider,
    rpc::{
        client::RpcClient,
        json_rpc::{RequestPacket, ResponsePacket},
    },
    transports::{utils::guess_local_url, TransportError, TransportErrorKind, TransportFut},
};
use reqwest::Url;

/// A provider of the node at `url` over HTTP, through [`NodeTransport`].
pub(super) fn node_provider(url: Url) -> RootProvider {
    let is_local = guess_local_url(url.as_str());
    RootProvider::new(RpcClient::new(
        NodeTransport { client: reqwest::Client::new(), url },
        is_local,
    ))
}

/// The node's JSON-RPC transport over HTTP: alloy's own, less the URL it records on every
/// request's span. A node's URL commonly carries a key, in its query or its userinfo, and that
/// span would print it to any log at debug level; nor does a failure here quote the URL.
#[derive(Clone)]
struct NodeTransport {
    client: reqwest::Client,
    url: Url,
}

impl tower_service::Service<RequestPacket> for NodeTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let response = this
                .client
                .post(this.url)
                .json(&req)
                .headers(req.headers())
                .send()
                .await
                .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
            let status = response.status();
            let body = response
                .bytes()
                .await
                .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
            if !status.is_success() {
                return Err(TransportErrorKind::http_error(
                    status.as_u16(),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
            }
            serde_json::from_slice(&body)
                .map_err(|e| TransportError::deser_err(e, String::from_utf8_lossy(&body)))
        })
    }
}
