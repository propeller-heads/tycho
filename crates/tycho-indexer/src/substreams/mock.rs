//! Generic mock for the Substreams Stream/Blocks gRPC service.
//!
//! Captures every `Request` protobuf sent by the client and answers it from a
//! script of [`MockResponse`]s, one per request. Requests beyond the end of the
//! script get an empty stream (trailers-only `grpc-status: 0`), which makes
//! `stream_blocks` yield `BlockResponse::Ended` and the runner exit cleanly.
use std::{
    collections::VecDeque,
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use prost::{bytes::Bytes, Message};
use tonic::{
    body::BoxBody,
    codegen::{
        http::{self, HeaderMap, HeaderValue},
        Body as HttpBody,
    },
    server::NamedService,
    Status,
};

use crate::pb::sf::substreams::rpc::{
    v2::{response::Message as ResponseMessage, BlockScopedData, Response},
    v3::Request,
};

/// gRPC status code for `Unauthenticated`, as sent on the wire.
const GRPC_STATUS_UNAUTHENTICATED: &str = "16";

/// How the mock answers a single incoming request.
pub enum MockResponse {
    /// Trailers-only `grpc-status: 0` — the client sees an empty stream.
    Ok,
    /// Trailers-only `grpc-status: 16` — rejected before any block was sent.
    Unauthenticated,
    /// One `BlockScopedData` carrying `cursor`, then `grpc-status: 16` trailers.
    BlockThenUnauthenticated { cursor: String },
}

/// Response body that emits one gRPC data frame and then error trailers.
///
/// A trailers-only response cannot express "the stream worked, then failed",
/// which is the shape the client sees when an endpoint rejects a stream that
/// was already delivering blocks.
struct DataThenTrailers {
    data: Option<Bytes>,
    grpc_status: &'static str,
}

impl HttpBody for DataThenTrailers {
    type Data = Bytes;
    type Error = Status;

    fn poll_data(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        Poll::Ready(self.get_mut().data.take().map(Ok))
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static(self.grpc_status));
        Poll::Ready(Ok(Some(trailers)))
    }
}

/// Wrap a protobuf message in a gRPC length-prefixed frame.
fn grpc_frame(message: &impl Message) -> Bytes {
    let payload = message.encode_to_vec();
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0); // not compressed
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Bytes::from(frame)
}

/// Mock gRPC server that captures Substreams `Request` messages.
///
/// Implements `tower::Service` directly — no generated server code needed.
/// Every incoming request is decoded from the gRPC wire format and pushed into
/// [`captured`], then answered from the script.
#[derive(Clone)]
pub struct MockSubstreamsServer {
    captured: Arc<Mutex<Vec<Request>>>,
    script: Arc<Mutex<VecDeque<MockResponse>>>,
}

impl MockSubstreamsServer {
    fn new(script: Vec<MockResponse>) -> (Self, Arc<Mutex<Vec<Request>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server =
            Self { captured: captured.clone(), script: Arc::new(Mutex::new(script.into())) };
        (server, captured)
    }
}

impl NamedService for MockSubstreamsServer {
    const NAME: &'static str = "sf.substreams.rpc.v3.Stream";
}

impl tonic::codegen::Service<http::Request<tonic::transport::Body>> for MockSubstreamsServer {
    type Response = http::Response<BoxBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<tonic::transport::Body>) -> Self::Future {
        let captured = self.captured.clone();
        let script = self.script.clone();
        Box::pin(async move {
            // Collect the request body using http_body::Body::poll_data
            let mut body = req.into_body();
            let mut buf = Vec::new();
            while let Some(chunk) =
                std::future::poll_fn(|cx| Pin::new(&mut body).poll_data(cx)).await
            {
                if let Ok(data) = chunk {
                    buf.extend_from_slice(&data);
                }
            }

            // gRPC frame: 1 byte compressed flag + 4 bytes length + protobuf message
            if buf.len() > 5 {
                if let Ok(request) = Request::decode(&buf[5..]) {
                    captured.lock().unwrap().push(request);
                }
            }

            let scripted = script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(MockResponse::Ok);

            let builder = http::Response::builder().header("content-type", "application/grpc");
            let response = match scripted {
                MockResponse::Ok => builder
                    .header("grpc-status", "0")
                    .body(BoxBody::default()),
                MockResponse::Unauthenticated => builder
                    .header("grpc-status", GRPC_STATUS_UNAUTHENTICATED)
                    .body(BoxBody::default()),
                MockResponse::BlockThenUnauthenticated { cursor } => {
                    let block = Response {
                        message: Some(ResponseMessage::BlockScopedData(BlockScopedData {
                            cursor,
                            ..Default::default()
                        })),
                    };
                    builder.body(BoxBody::new(DataThenTrailers {
                        data: Some(grpc_frame(&block)),
                        grpc_status: GRPC_STATUS_UNAUTHENTICATED,
                    }))
                }
            };

            Ok(response.unwrap())
        })
    }
}

/// Start a mock Substreams gRPC server that answers the n-th request with the
/// n-th entry of `script`, and an empty stream once the script runs out.
///
/// Returns the captured requests and the address the server is listening on.
pub async fn start_scripted_mock_substreams(
    script: Vec<MockResponse>,
) -> (Arc<Mutex<Vec<Request>>>, SocketAddr) {
    let (server, captured) = MockSubstreamsServer::new(script);

    // Bind to find an available port, then release so tonic can rebind.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(server)
            .serve(addr)
            .await
            .unwrap();
    });

    // Give the server a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (captured, addr)
}
