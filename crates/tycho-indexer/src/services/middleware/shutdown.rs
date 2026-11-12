use actix_web::{
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    http::ConnectionType,
    middleware::Next,
    web,
};
use tokio::sync::watch;

/// Turns `true` when the process starts to shut down.
#[derive(Clone)]
pub(in crate::services) struct ShutdownSignal(pub(in crate::services) watch::Receiver<bool>);

/// Closes the HTTP/1 connection after the response once the process shuts down.
///
/// The server stops accepting connections on the shutdown signal, but serves requests on open
/// keep-alive connections until it stops. The view of a stopping process does not move any more,
/// so a client asking for a newer block gets an error. Closing the connection makes the next
/// request of the client open a new connection, which goes to an instance that still runs.
///
/// Passes every response through unchanged while the app has no [`ShutdownSignal`] or the
/// process runs.
pub(in crate::services) async fn close_connection_on_shutdown<B>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<B>, actix_web::Error>
where
    B: MessageBody + 'static,
{
    let shutting_down = req
        .app_data::<web::Data<ShutdownSignal>>()
        .is_some_and(|signal| *signal.0.borrow());
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

    use super::*;

    async fn ok() -> HttpResponse {
        HttpResponse::Ok().finish()
    }

    async fn connection_type(signal: Option<ShutdownSignal>) -> ConnectionType {
        let mut app = App::new()
            .wrap(from_fn(close_connection_on_shutdown))
            .route("/", web::get().to(ok));
        if let Some(signal) = signal {
            app = app.app_data(web::Data::new(signal));
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

        assert_eq!(connection_type(Some(ShutdownSignal(rx))).await, ConnectionType::KeepAlive);
    }

    #[actix_web::test]
    async fn test_closes_the_connection_once_the_process_shuts_down() {
        let (tx, rx) = watch::channel(false);
        tx.send_replace(true);

        assert_eq!(connection_type(Some(ShutdownSignal(rx))).await, ConnectionType::Close);
    }

    #[actix_web::test]
    async fn test_keeps_the_connection_without_a_shutdown_signal() {
        assert_eq!(connection_type(None).await, ConnectionType::KeepAlive);
    }
}
