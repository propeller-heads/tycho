use std::collections::HashMap;

use alloy::primitives::{aliases::U24, Address};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{get_static_attribute, pad_or_truncate_to_size},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct FluidV2SwapEncoder {
    executor_address: Bytes,
    native_address: Bytes,
}

impl SwapEncoder for FluidV2SwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self {
            executor_address,
            native_address: Bytes::from("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"),
        })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let token_in = self.coerce_native_address(&swap.token_in().address);
        let token_out = self.coerce_native_address(&swap.token_out().address);

        if token_in == token_out {
            return Err(EncodingError::InvalidInput("Fluid v2 swap tokens must differ".to_string()));
        }

        let token_in_addr = self.bytes_to_address(token_in)?;
        let token_out_addr = self.bytes_to_address(token_out)?;
        let (token0, token1, swap0_to1) = if token_in_addr < token_out_addr {
            (token_in_addr, token_out_addr, true)
        } else {
            (token_out_addr, token_in_addr, false)
        };

        let dex_type = *pad_or_truncate_to_size::<1>(&get_static_attribute(swap, "dex_type")?)?
            .first()
            .ok_or_else(|| EncodingError::FatalError("dex_type is empty".to_string()))?;

        let fee =
            U24::from_be_bytes(pad_or_truncate_to_size::<3>(&get_static_attribute(swap, "fee")?)?);
        let tick_spacing = U24::from_be_bytes(pad_or_truncate_to_size::<3>(
            &get_static_attribute(swap, "tick_spacing")?,
        )?);

        // The controller is part of the dex key, so a component without it names no pool.
        let controller =
            self.bytes_to_address(&Bytes::from(get_static_attribute(swap, "controller")?))?;

        let controller_data = swap
            .user_data()
            .clone()
            .unwrap_or_default();

        let mut encoded = Vec::with_capacity(68 + controller_data.len());
        encoded.extend([dex_type]);
        encoded.extend(token0);
        encoded.extend(token1);
        encoded.extend(fee.to_be_bytes::<3>());
        encoded.extend(tick_spacing.to_be_bytes::<3>());
        encoded.extend(controller);
        encoded.extend([u8::from(swap0_to1)]);
        encoded.extend(controller_data);

        Ok(encoded)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

impl FluidV2SwapEncoder {
    fn coerce_native_address<'a>(&'a self, address: &'a Bytes) -> &'a Bytes {
        if address.is_empty() || *address == Bytes::zero(20) {
            &self.native_address
        } else {
            address
        }
    }

    fn bytes_to_address(&self, address: &Bytes) -> Result<Address, EncodingError> {
        if address.len() != 20 {
            return Err(EncodingError::InvalidInput(format!("Invalid address: {address}")));
        }

        Ok(Address::from_slice(address))
    }
}

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";

    fn pool(static_attributes: &[(&str, &str)]) -> ProtocolComponent {
        ProtocolComponent {
            protocol_system: String::from("fluid_v2"),
            static_attributes: static_attributes
                .iter()
                .map(|(name, value)| (name.to_string(), Bytes::from(*value)))
                .collect(),
            ..Default::default()
        }
    }

    fn encode_swap(
        component: ProtocolComponent,
        token_in: &str,
        token_out: &str,
    ) -> Result<String, EncodingError> {
        let token_in = Bytes::from(token_in);
        let token_out = Bytes::from(token_out);
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: token_in,
            group_token_out: token_out,
            origin: None,
        };
        let encoder = FluidV2SwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None)?;
        encoder
            .encode_swap(&swap, &encoding_context)
            .map(encode)
    }

    #[test]
    fn test_encode_orders_tokens_by_address() {
        let component = pool(&[
            ("dex_type", "0x04"),
            ("fee", "0x64"),
            ("tick_spacing", "0x01"),
            ("controller", "0x0000000000000000000000000000000000000000"),
        ]);

        let encoded = encode_swap(component, USDT, USDC).unwrap();

        assert_eq!(
            encoded,
            String::from(concat!(
                "04",                                       // dex type
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token0
                "dac17f958d2ee523a2206206994597c13d831ec7", // token1
                "000064",                                   // fee
                "000001",                                   // tick spacing
                "0000000000000000000000000000000000000000", // controller
                "00",                                       // swap0To1
            ))
        );
    }

    #[test]
    fn test_encode_native_token_as_fluid_sentinel() {
        let component = pool(&[
            ("dex_type", "0x03"),
            ("fee", "0x01f4"),
            ("tick_spacing", "0x0a"),
            ("controller", "0x0000000000000000000000000000000000000000"),
        ]);

        let encoded =
            encode_swap(component, USDC, "0x0000000000000000000000000000000000000000").unwrap();

        assert_eq!(&encoded[2..42], &USDC[2..]);
        assert_eq!(&encoded[42..82], "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        assert_eq!(&encoded[82..88], "0001f4");
        assert_eq!(&encoded[134..136], "01");
    }

    #[test]
    fn test_encode_dynamic_fee_from_signed_bytes() {
        // Substreams writes the uint24 0xFFFFFF as a signed big-endian integer.
        let component = pool(&[
            ("dex_type", "0x03"),
            ("fee", "0x00ffffff"),
            ("tick_spacing", "0x0a"),
            ("controller", "0x0000000000000000000000000000000000000000"),
        ]);

        let encoded = encode_swap(component, USDC, USDT).unwrap();

        assert_eq!(&encoded[82..88], "ffffff");
    }

    #[test]
    fn test_encode_appends_controller_data() {
        let component = pool(&[
            ("dex_type", "0x03"),
            ("fee", "0x64"),
            ("tick_spacing", "0x01"),
            ("controller", "0x0000000000000000000000000000000000000001"),
        ]);
        let dai = Bytes::from("0x6b175474e89094c44da98b954eedeac495271d0f");
        let usdc = Bytes::from(USDC);
        let swap = Swap::new(
            component,
            default_token(dai.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        )
        .with_user_data(Bytes::from("0x1234"));
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: dai,
            group_token_out: usdc,
            origin: None,
        };
        let encoder = FluidV2SwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();

        let encoded = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();

        assert_eq!(
            encode(encoded),
            concat!(
                "03",
                "6b175474e89094c44da98b954eedeac495271d0f",
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "000064",
                "000001",
                "0000000000000000000000000000000000000001",
                "01",
                "1234"
            )
        );
    }

    #[rstest::rstest]
    #[case::dex_type("dex_type")]
    #[case::fee("fee")]
    #[case::tick_spacing("tick_spacing")]
    #[case::controller("controller")]
    fn test_encode_without_dex_key_attribute_fails(#[case] missing: &str) {
        let attributes: Vec<_> = [
            ("dex_type", "0x03"),
            ("fee", "0x64"),
            ("tick_spacing", "0x01"),
            ("controller", "0x0000000000000000000000000000000000000000"),
        ]
        .into_iter()
        .filter(|(name, _)| *name != missing)
        .collect();

        assert!(encode_swap(pool(&attributes), USDC, USDT).is_err());
    }

    #[test]
    fn test_encode_same_token_fails() {
        let component = pool(&[("dex_type", "0x03"), ("fee", "0x64"), ("tick_spacing", "0x01")]);

        assert!(encode_swap(component, USDC, USDC).is_err());
    }
}
