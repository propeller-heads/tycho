use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_common::Bytes;

use super::hook_handler::PonsV2HookHandler;
use crate::{
    evm::protocol::uniswap_v4::hooks::{
        hook_handler::HookHandler,
        hook_handler_creator::{HookCreationParams, HookHandlerCreator},
    },
    protocol::errors::InvalidSnapshotError,
};

/// `_registerPool` rejects a pool whose `hookFeeBps` exceeds this.
const MAX_HOOK_FEE_BPS: u32 = 1_000;

/// `_registerPool` rejects a pool whose `creatorTaxBps + hookFeeBps` exceeds this. There is no
/// separate ceiling on the creator tax, so it may reach the full 2,000 when the hook fee is zero.
const MAX_TOTAL_TRADE_FEE_BPS: u32 = 2_000;

const HOOK_FEE_ATTRIBUTE: &str = "pons_hook_fee_bps";
const CREATOR_TAX_ATTRIBUTE: &str = "pons_creator_tax_bps";

pub struct PonsV2HookCreator;

/// Reads one big-endian unsigned attribute written by `BigInt::to_signed_bytes_be`, which is
/// minimal-width: one byte for 0 and 100, two from 200 up.
///
/// Anything wider than eight bytes, or past the `uint16` the contract stores the value in, is
/// rejected rather than truncated.
fn bps_attribute(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<u16, InvalidSnapshotError> {
    let raw = attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))?;

    if raw.len() > 8 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes (0x{}), which cannot be a basis-point value",
            raw.len(),
            hex::encode(raw)
        )));
    }

    let value = U256::from_be_slice(raw);
    u16::try_from(value).map_err(|_| {
        InvalidSnapshotError::ValueError(format!(
            "{name} = {value} does not fit the uint16 the contract stores it in"
        ))
    })
}

impl HookHandlerCreator for PonsV2HookCreator {
    /// Builds a [`PonsV2HookHandler`] from the pool's `pons_hook_fee_bps` and
    /// `pons_creator_tax_bps` attributes.
    ///
    /// Rejects any pair the deployed `_registerPool` would itself have rejected, so a snapshot
    /// that disagrees with the contract fails to decode instead of quoting a fee no pool charges.
    fn instantiate_hook_handler(
        &self,
        params: HookCreationParams<'_>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        let hook_fee_bps = bps_attribute(params.attributes, HOOK_FEE_ATTRIBUTE)?;
        let creator_tax_bps = bps_attribute(params.attributes, CREATOR_TAX_ATTRIBUTE)?;

        if u32::from(hook_fee_bps) > MAX_HOOK_FEE_BPS {
            return Err(InvalidSnapshotError::ValueError(format!(
                "{HOOK_FEE_ATTRIBUTE} = {hook_fee_bps} exceeds the contract maximum of \
                 {MAX_HOOK_FEE_BPS}"
            )));
        }
        // Widened to u32 so that two u16 attributes can never overflow their sum.
        let combined = u32::from(hook_fee_bps) + u32::from(creator_tax_bps);
        if combined > MAX_TOTAL_TRADE_FEE_BPS {
            return Err(InvalidSnapshotError::ValueError(format!(
                "{CREATOR_TAX_ATTRIBUTE} = {creator_tax_bps} plus {HOOK_FEE_ATTRIBUTE} = \
                 {hook_fee_bps} is {combined}, above the contract maximum of \
                 {MAX_TOTAL_TRADE_FEE_BPS}"
            )));
        }

        Ok(Box::new(PonsV2HookHandler::new(params.hook_address(), hook_fee_bps, creator_tax_bps)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::{address, Address, U256};
    use rstest::rstest;
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use super::*;
    use crate::{
        evm::protocol::uniswap_v4::{
            hooks::{
                hook_handler::HookHandler,
                hook_handler_creator::{
                    initialize_hook_handlers, instantiate_hook_handler, HookCreationParams,
                    HookHandlerCreator,
                },
                pons_v2::hook_handler::{PonsV2HookHandler, PONS_V2_HOOK_ROBINHOOD},
            },
            state::{UniswapV4Fees, UniswapV4State},
        },
        protocol::errors::InvalidSnapshotError,
    };

    /// Owns the collections a [`HookCreationParams`] borrows.
    struct Snapshot {
        account_balances: HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: HashMap<Bytes, Token>,
        attributes: HashMap<String, Bytes>,
        balances: HashMap<Bytes, Bytes>,
    }

    impl Snapshot {
        fn new(hook_fee_bps: Option<&[u8]>, creator_tax_bps: Option<&[u8]>) -> Self {
            let mut attributes = HashMap::new();
            if let Some(value) = hook_fee_bps {
                attributes.insert("pons_hook_fee_bps".to_string(), Bytes::from(value.to_vec()));
            }
            if let Some(value) = creator_tax_bps {
                attributes.insert("pons_creator_tax_bps".to_string(), Bytes::from(value.to_vec()));
            }
            Self {
                account_balances: HashMap::new(),
                all_tokens: HashMap::new(),
                attributes,
                balances: HashMap::new(),
            }
        }

        fn params(&self, hook: Address) -> HookCreationParams<'_> {
            let state = UniswapV4State::new(
                0,
                U256::from(1),
                UniswapV4Fees::new(0, 0, 0),
                0,
                1,
                Vec::new(),
            )
            .expect("a bare pool state builds");
            HookCreationParams::new(
                hook,
                &self.account_balances,
                &self.all_tokens,
                state,
                &self.attributes,
                &self.balances,
                None,
            )
        }
    }

    fn instantiate(
        hook_fee_bps: Option<&[u8]>,
        creator_tax_bps: Option<&[u8]>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        let snapshot = Snapshot::new(hook_fee_bps, creator_tax_bps);
        PonsV2HookCreator.instantiate_hook_handler(snapshot.params(PONS_V2_HOOK_ROBINHOOD))
    }

    /// The substreams module writes both bps with `BigInt::to_signed_bytes_be`, which is
    /// minimal-width and therefore one byte for 0 and 100, two for 200 and above.
    #[rstest]
    #[case::hundred_and_two_hundred(&[0x64], &[0x00, 0xc8], 100, 200)]
    #[case::zero_tax(&[0x64], &[0x00], 100, 0)]
    #[case::both_zero(&[0x00], &[0x00], 0, 0)]
    #[case::max_hook_fee(&[0x03, 0xe8], &[0x03, 0xe8], 1_000, 1_000)]
    #[case::max_creator_tax(&[0x00], &[0x07, 0xd0], 0, 2_000)]
    #[case::leading_zero_padding(&[0x00, 0x00, 0x00, 0x64], &[0x00], 100, 0)]
    fn builds_the_handler_from_the_pool_attributes(
        #[case] hook_fee_bps: &[u8],
        #[case] creator_tax_bps: &[u8],
        #[case] expected_fee: u16,
        #[case] expected_tax: u16,
    ) {
        let handler = instantiate(Some(hook_fee_bps), Some(creator_tax_bps))
            .expect("in-range attributes should build a handler");

        let expected = PonsV2HookHandler::new(PONS_V2_HOOK_ROBINHOOD, expected_fee, expected_tax);
        assert!(handler.is_equal(&expected));
        assert_eq!(handler.address(), PONS_V2_HOOK_ROBINHOOD);
    }

    #[rstest]
    #[case::missing_hook_fee(None, Some(&[0x00][..]), "pons_hook_fee_bps")]
    #[case::missing_creator_tax(Some(&[0x64][..]), None, "pons_creator_tax_bps")]
    fn a_missing_attribute_is_reported_by_name(
        #[case] hook_fee_bps: Option<&[u8]>,
        #[case] creator_tax_bps: Option<&[u8]>,
        #[case] expected: &str,
    ) {
        let error = instantiate(hook_fee_bps, creator_tax_bps)
            .expect_err("a Pons pool without its bps cannot be simulated");

        let InvalidSnapshotError::MissingAttribute(name) = error else {
            panic!("expected a missing-attribute error, got {error:?}");
        };
        assert_eq!(name, expected);
    }

    /// `_registerPool` enforces `hookFeeBps <= 1_000` and
    /// `creatorTaxBps + hookFeeBps <= 2_000`, with no standalone cap on the creator tax.
    #[rstest]
    #[case::hook_fee_over_its_cap(&[0x03, 0xe9], &[0x00], "pons_hook_fee_bps")]
    #[case::combined_over_the_cap(&[0x00], &[0x07, 0xd1], "pons_creator_tax_bps")]
    #[case::combined_over_the_cap_by_one(&[0x01], &[0x07, 0xd0], "pons_creator_tax_bps")]
    #[case::hook_fee_far_over(&[0x27, 0x10], &[0x00], "pons_hook_fee_bps")]
    fn out_of_range_bps_are_rejected(
        #[case] hook_fee_bps: &[u8],
        #[case] creator_tax_bps: &[u8],
        #[case] expected_name: &str,
    ) {
        let error = instantiate(Some(hook_fee_bps), Some(creator_tax_bps))
            .expect_err("the deployed contract could never have registered these terms");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains(expected_name), "{message}");
    }

    #[rstest]
    #[case::nine_bytes(&[0u8; 9][..])]
    #[case::thirty_two_bytes(&[0xffu8; 32][..])]
    #[case::eight_bytes_past_uint16(&[0xffu8; 8][..])]
    fn an_oversized_attribute_is_rejected(#[case] hook_fee_bps: &[u8]) {
        let error = instantiate(Some(hook_fee_bps), Some(&[0x00]))
            .expect_err("a bps value is at most two bytes on chain");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("pons_hook_fee_bps"), "{message}");
    }

    /// The creator tax is checked only against the combined cap, so an attribute that is far
    /// past `uint16` has to be rejected on its own rather than overflowing the sum.
    #[test]
    fn a_creator_tax_past_uint16_is_rejected_before_the_combined_check() {
        let error = instantiate(Some(&[0x64]), Some(&[0xffu8; 8]))
            .expect_err("no pool can carry a creator tax of 2^64 - 1");

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(message.contains("pons_creator_tax_bps"), "{message}");
    }

    #[test]
    fn the_handler_takes_the_hook_address_from_the_creation_params() {
        let other = address!("00000000000000000000000000000000000000aa");
        let snapshot = Snapshot::new(Some(&[0x64]), Some(&[0x00]));

        let handler = PonsV2HookCreator
            .instantiate_hook_handler(snapshot.params(other))
            .expect("in-range attributes should build a handler");

        assert_eq!(handler.address(), other);
    }

    #[test]
    fn the_creator_is_registered_for_pons_on_robinhood() {
        initialize_hook_handlers().expect("hook handler registration should succeed");
        let snapshot = Snapshot::new(Some(&[0x64]), Some(&[0x00, 0xc8]));

        let handler = instantiate_hook_handler(
            Chain::Robinhood,
            &PONS_V2_HOOK_ROBINHOOD,
            snapshot.params(PONS_V2_HOOK_ROBINHOOD),
        )
        .expect("the Pons creator should serve its own chain and address");

        let native = handler
            .as_any()
            .downcast_ref::<PonsV2HookHandler>()
            .expect("the registry should hand back the native Pons handler");
        assert_eq!(native.hook_fee_bps(), 100);
        assert_eq!(native.creator_tax_bps(), 200);
    }

    /// Pons is deployed on Robinhood alone, so the same address elsewhere is just another hook.
    #[test]
    fn the_creator_does_not_serve_the_same_address_on_another_chain() {
        initialize_hook_handlers().expect("hook handler registration should succeed");
        let snapshot = Snapshot::new(Some(&[0x64]), Some(&[0x00, 0xc8]));

        let error = instantiate_hook_handler(
            Chain::Ethereum,
            &PONS_V2_HOOK_ROBINHOOD,
            snapshot.params(PONS_V2_HOOK_ROBINHOOD),
        )
        .expect_err("the generic VM creator runs instead and wants its own attributes");

        let InvalidSnapshotError::MissingAttribute(name) = error else {
            panic!("expected the generic VM creator to run, got {error:?}");
        };
        assert_eq!(name, "balance_owner");
    }
}
