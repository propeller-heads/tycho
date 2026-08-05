use async_trait::async_trait;
use futures::stream::BoxStream;
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote,
};

use crate::{protocol::models::Update, rfq::errors::RFQError};

#[async_trait]
pub trait RFQClient: Send + Sync {
    /// Returns a stream of state updates.
    ///
    /// Each update carries fully constructed protocol states, ready for simulation. Streamed
    /// states embed this client's configuration (credentials, origin identification, timeouts),
    /// so binding quotes requested through them behave exactly like on this client.
    fn stream(&self) -> BoxStream<'static, Result<Update, RFQError>>;

    // This method is responsible for fetching the binding quote from the RFQ API. Use sender and
    // receiver from GetAmountOutParams to ask for the quote
    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError>;
}
