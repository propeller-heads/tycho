use std::{
    future::{ready, Future, Ready},
    pin::Pin,
};

use actix_web::{
    body::BoxBody,
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    Error, HttpResponse,
};

pub struct AccessControl {
    required_key: String,
}

impl AccessControl {
    pub fn new(key: &str) -> Self {
        Self { required_key: key.to_string() }
    }
}

impl<S> Transform<S, ServiceRequest> for AccessControl
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = Error> + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Transform = AccessControlMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(AccessControlMiddleware { service, required_key: self.required_key.clone() }))
    }
}

pub struct AccessControlMiddleware<S> {
    service: S,
    required_key: String,
}

impl<S> Service<ServiceRequest> for AccessControlMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = Error> + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let required_key = self.required_key.clone();

        if let Some(header_value) = req.headers().get("Authorization") {
            if header_value == required_key.as_str() {
                let fut = self.service.call(req);

                // More problems occur if we try to fix this warning
                #[expect(clippy::redundant_async_block)]
                return Box::pin(async move { fut.await });
            }
        }

        let response = HttpResponse::Unauthorized()
            .body("Access denied")
            .map_into_boxed_body();

        Box::pin(async move { Ok(req.into_response(response)) })
    }
}
