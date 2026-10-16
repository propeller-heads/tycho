use async_trait::async_trait;
use futures::stream::BoxStream;
use tycho_client::feed::synchronizer::StateSyncMessage;
use tycho_common::{
    models::protocol::GetAmountOutParams, simulation::indicatively_priced::SignedQuote,
};

use crate::rfq::{
    errors::RFQError,
    models::{ComponentLayout, TimestampHeader},
};

#[async_trait]
pub trait RFQClient: Send + Sync {
    /// Returns a stream of updates tagged with the provider name.
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>>;

    /// The components this client streams. The default is one per pair.
    fn component_layout(&self) -> ComponentLayout {
        ComponentLayout::PerPair
    }

    // This method is responsible for fetching the binding quote from the RFQ API. Use sender and
    // receiver from GetAmountOutParams to ask for the quote
    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError>;
}
