use actix_web::{
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    http::ConnectionType,
    middleware::Next,
    web,
};
use tycho_common::{storage::Gateway, traits::EntryPointTracer};

use crate::services::rpc::RpcHandler;

/// Closes the HTTP/1 connection after the response once the process shuts down.
///
/// Once the process stops, its view does not move any more, so a client asking for a newer block
/// gets an error. Closing the connection makes the next request of the client open a new
/// connection, which goes to an instance that still runs.
///
/// Passes every response through unchanged while the process runs, and in an app without a
/// [`RpcHandler`].
pub(in crate::services) async fn close_connection_on_shutdown<G, T, B>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<B>, actix_web::Error>
where
    G: Gateway + 'static,
    T: EntryPointTracer + Sync + 'static,
    B: MessageBody + 'static,
{
    let shutting_down = req
        .app_data::<web::Data<RpcHandler<G, T>>>()
        .is_some_and(|rpc| rpc.is_shutting_down());
    let mut response = next.call(req).await?;
    if shutting_down {
        response
            .response_mut()
            .head_mut()
            .set_connection_type(ConnectionType::Close);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use actix_web::{middleware::from_fn, test, App, HttpResponse};
    use tokio::sync::watch;
    use tycho_common::traits::MockEntryPointTracer;

    use super::*;
    use crate::{services::middleware::PlansConfig, testing::MockGateway};

    type Handler = RpcHandler<MockGateway, MockEntryPointTracer>;

    async fn ok() -> HttpResponse {
        HttpResponse::Ok().finish()
    }

    fn handler(shutdown: Option<watch::Receiver<bool>>) -> Handler {
        RpcHandler::new(
            MockGateway::new(),
            None,
            MockEntryPointTracer::new(),
            PlansConfig::default(),
            vec![],
            vec![],
            None,
        )
        .with_shutdown_signal(shutdown)
    }

    async fn connection_type(handler: Option<Handler>) -> ConnectionType {
        let mut app = App::new()
            .wrap(from_fn(close_connection_on_shutdown::<MockGateway, MockEntryPointTracer, _>))
            .route("/", web::get().to(ok));
        if let Some(handler) = handler {
            app = app.app_data(web::Data::new(handler));
        }
        let app = test::init_service(app).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/")
                .to_request(),
        )
        .await;
        response
            .response()
            .head()
            .connection_type()
    }

    #[actix_web::test]
    async fn test_keeps_the_connection_while_the_process_runs() {
        let (_tx, rx) = watch::channel(false);

        assert_eq!(connection_type(Some(handler(Some(rx)))).await, ConnectionType::KeepAlive);
    }

    #[actix_web::test]
    async fn test_closes_the_connection_once_the_process_shuts_down() {
        let (tx, rx) = watch::channel(false);
        tx.send_replace(true);

        assert_eq!(connection_type(Some(handler(Some(rx)))).await, ConnectionType::Close);
    }

    #[actix_web::test]
    async fn test_keeps_the_connection_without_a_shutdown_signal() {
        assert_eq!(connection_type(Some(handler(None))).await, ConnectionType::KeepAlive);
    }

    #[actix_web::test]
    async fn test_keeps_the_connection_without_a_rpc_handler() {
        assert_eq!(connection_type(None).await, ConnectionType::KeepAlive);
    }
}
