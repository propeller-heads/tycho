use std::{collections::HashMap, str::FromStr};

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Static attribute under which price-level-stream components carry their pAMM venue address.
const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// Suffix of the protocol-specific config key holding a venue's address, prefixed with the venue
/// name: `fermiswap_venue_address`.
const VENUE_ADDRESS_CONFIG_SUFFIX: &str = "_venue_address";

/// Encodes a swap on any pAMM implementing the standard `IPropAMM` interface pushed by Titan.
///
/// A single generic executor serves all such venues, so the pAMM address travels in the swap data
/// (packed `pamm ++ token_in ++ token_out`). It comes from the component's `pamm_address` static
/// attribute, which every price-level-stream component carries, or, for components that carry no
/// such attribute, from the venue address configured for the venue named by the protocol system.
#[derive(Clone)]
pub struct PropAMMSwapEncoder {
    executor_address: Bytes,
    /// Venue address per venue name, from `protocol_specific_addresses.json`. Serves components
    /// sourced outside the price level stream, which carry no `pamm_address` attribute: the
    /// indexed `vm:fermiswap` path describes the same venue but names only its swapper contract.
    configured_venues: HashMap<String, Bytes>,
}

impl PropAMMSwapEncoder {
    fn parse_configured_venues(
        config: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, Bytes>, EncodingError> {
        let mut venues = HashMap::new();
        for (key, address) in config.unwrap_or_default() {
            let Some(venue) = key.strip_suffix(VENUE_ADDRESS_CONFIG_SUFFIX) else {
                continue;
            };
            let address = Bytes::from_str(&address).map_err(|_| {
                EncodingError::FatalError(format!(
                    "Invalid pAMM venue address for {venue}: {address}"
                ))
            })?;
            if address.len() != 20 {
                return Err(EncodingError::FatalError(format!(
                    "pAMM venue address for {venue} is not 20 bytes: {address}"
                )));
            }
            venues.insert(venue.to_string(), address);
        }
        Ok(venues)
    }

    /// The venue to swap against: the component's `pamm_address` attribute, else the address
    /// configured for the venue its protocol system names.
    fn venue_address(&self, swap: &Swap) -> Result<Bytes, EncodingError> {
        let component = swap.component();
        if let Some(pamm) = component
            .static_attributes
            .get(PAMM_ADDRESS_ATTRIBUTE)
        {
            return Ok(pamm.clone());
        }

        let venue = component
            .protocol_system
            .split_once(':')
            .map(|(_, venue)| venue)
            .unwrap_or_default();
        self.configured_venues
            .get(venue)
            .cloned()
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "pAMM component {} carries no {PAMM_ADDRESS_ATTRIBUTE} static attribute and \
                     no venue address is configured for {venue}",
                    component.id
                ))
            })
    }
}

impl SwapEncoder for PropAMMSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "Price level stream swaps are only supported on Ethereum".to_string(),
            ));
        }

        Ok(Self { executor_address, configured_venues: Self::parse_configured_venues(config)? })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let pamm = bytes_to_address(&self.venue_address(swap)?)?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let args = (pamm, token_in, token_out);
        Ok(args.abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    // Must match what the Solidity integration test decodes the generated calldata against
    // (`contracts/test/protocols/PropAMM.t.sol`: `MOCK_PAMM`, `WETH_ADDR`, `USDC_ADDR`).
    const PAMM: &str = "1111111111111111111111111111111111111111";
    // The FermiSwap pAMM on the PropAMMRouter whitelist, not the FermiSwapper the indexed path
    // calls.
    const FERMI_VENUE: &str = "5979458912f80b96d30d4220af8e2e4925a33320";
    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn weth_usdc_component() -> ProtocolComponent {
        ProtocolComponent {
            // The id the price level stream produces: pamm ++ token0 ++ token1.
            id: format!("0x{PAMM}{WETH}{USDC}"),
            protocol_system: String::from("pricelevelstream:kipseli"),
            static_attributes: HashMap::from([(
                PAMM_ADDRESS_ATTRIBUTE.to_string(),
                Bytes::from(format!("0x{PAMM}").as_str()),
            )]),
            ..Default::default()
        }
    }

    fn encoder() -> PropAMMSwapEncoder {
        PropAMMSwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap()
    }

    fn encoder_with_venue_config() -> PropAMMSwapEncoder {
        PropAMMSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(
                "fermiswap_venue_address".to_string(),
                format!("0x{FERMI_VENUE}"),
            )])),
        )
        .unwrap()
    }

    /// A component shaped like the indexed `vm:fermiswap` path: an opaque id, no `pamm_address`
    /// attribute, relabeled onto the PropAMMRouter family by the caller.
    fn indexed_fermiswap_component() -> ProtocolComponent {
        ProtocolComponent {
            id: String::from("0x7c85004568584fbf3665f41ebe85146ee0483587d65d9ea5a56c79816bb720d0"),
            protocol_system: String::from("propammfallback:fermiswap"),
            ..Default::default()
        }
    }

    fn encode_weth_usdc(component: ProtocolComponent) -> String {
        encode_weth_usdc_with(&encoder(), component)
    }

    fn encode_weth_usdc_with(encoder: &PropAMMSwapEncoder, component: ProtocolComponent) -> String {
        let token_in = Bytes::from(format!("0x{WETH}").as_str());
        let token_out = Bytes::from(format!("0x{USDC}").as_str());
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        encode(&encoded_swap)
    }

    #[test]
    fn test_encode_propamm_weth_usdc() {
        let hex_swap = encode_weth_usdc(weth_usdc_component());

        assert_eq!(hex_swap, format!("{PAMM}{WETH}{USDC}"));
        write_calldata_to_file("test_encode_propamm_weth_usdc", hex_swap.as_str());
    }

    #[test]
    fn test_rejects_component_without_pamm_address() {
        let mut component = weth_usdc_component();
        component.static_attributes.clear();
        let swap = Swap::new(
            component,
            default_token(Bytes::from(format!("0x{WETH}").as_str())),
            default_token(Bytes::from(format!("0x{USDC}").as_str())),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(format!("0x{WETH}").as_str()),
            group_token_out: Bytes::from(format!("0x{USDC}").as_str()),
        };

        let result = encoder().encode_swap(&swap, &encoding_context);
        assert!(result.is_err());
    }

    /// A component with no `pamm_address` attribute takes the venue configured for the venue its
    /// protocol system names. This is what lets the indexed `vm:fermiswap` path reach the
    /// PropAMMRouter: that path knows only the FermiSwapper contract, never the pAMM.
    #[test]
    fn test_encode_propamm_venue_from_config() {
        let hex_swap =
            encode_weth_usdc_with(&encoder_with_venue_config(), indexed_fermiswap_component());

        assert_eq!(hex_swap, format!("{FERMI_VENUE}{WETH}{USDC}"));
        write_calldata_to_file("test_encode_propamm_fallback_indexed_fermiswap", hex_swap.as_str());
    }

    /// The component's own attribute wins, so configuring a venue never overrides a
    /// price-level-stream component.
    #[test]
    fn test_static_attribute_wins_over_configured_venue() {
        let hex_swap = encode_weth_usdc_with(&encoder_with_venue_config(), weth_usdc_component());

        assert_eq!(hex_swap, format!("{PAMM}{WETH}{USDC}"));
    }

    /// A venue with no attribute and no configured address is an encoding error, not a swap
    /// against `address(0)`.
    #[test]
    fn test_rejects_unconfigured_venue_without_attribute() {
        let mut component = indexed_fermiswap_component();
        component.protocol_system = String::from("propammfallback:unknown");
        let token_in = Bytes::from(format!("0x{WETH}").as_str());
        let token_out = Bytes::from(format!("0x{USDC}").as_str());
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        let result = encoder_with_venue_config().encode_swap(&swap, &encoding_context);
        assert!(result.is_err());
    }

    #[test]
    fn test_rejects_malformed_configured_venue() {
        let result = PropAMMSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(
                "fermiswap_venue_address".to_string(),
                "0xdeadbeef".to_string(),
            )])),
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_encoder_rejects_non_ethereum_chain() {
        let result = PropAMMSwapEncoder::new(Bytes::zero(20), Chain::Base, None);
        assert!(result.is_err());
    }
}
