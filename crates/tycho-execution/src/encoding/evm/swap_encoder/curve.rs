use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::{Address, U8},
    sol_types::SolValue,
};
use serde_json::from_str;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::ROUTER_ETH_ADDRESS,
        utils::{bytes_to_address, get_static_attribute},
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Curve pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
/// * `native_token_address` - The address of the native token.
#[derive(Clone)]
pub struct CurveSwapEncoder {
    executor_address: Bytes,
    native_token_address: Bytes,
    wrapped_native_token_address: Bytes,
}

impl CurveSwapEncoder {
    fn get_pool_type(&self, pool_id: &str, factory_address: &str) -> Result<U8, EncodingError> {
        match pool_id {
            // TriPool
            "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7" => Ok(U8::from(1)),
            // STETHPool
            "0xDC24316b9AE028F1497c275EB9192a3Ea0f67022" => Ok(U8::from(1)),
            // TriCryptoPool
            "0xD51a44d3FaE010294C616388b506AcdA1bfAAE46" => Ok(U8::from(3)),
            // SUSDPool
            "0xA5407eAE9Ba41422680e2e00537571bcC53efBfD" => Ok(U8::from(1)),
            // FRAXUSDCPool
            "0xDcEF968d416a41Cdac0ED8702fAC8128A64241A2" => Ok(U8::from(1)),
            _ => match factory_address {
                // CryptoSwapNG factory
                "0x6A8cbed756804B16E05E741eDaBd5cB544AE21bf" => Ok(U8::from(1)),
                // Metapool factory
                "0xB9fC157394Af804a3578134A6585C0dc9cc990d4" => Ok(U8::from(1)),
                // CryptoPool factory
                "0xF18056Bbd320E96A48e3Fbf8bC061322531aac99" => Ok(U8::from(2)),
                // Tricrypto factory
                "0x0c0e5f2fF0ff18a3be9b835635039256dC4B4963" => Ok(U8::from(3)),
                // Twocrypto factory
                "0x98EE851a00abeE0d95D08cF4CA2BdCE32aeaAF7F" => Ok(U8::from(2)),
                // StableSwap factory
                "0x4F8846Ae9380B90d2E71D5e3D042dff3E7ebb40d" => Ok(U8::from(1)),
                // Unichain Tricrypto factory
                "0x5702BDB1Ec244704E3cBBaAE11a0275aE5b07499" => Ok(U8::from(3)),
                // Unichain Twocrypto factory
                "0xc9Fe0C63Af9A39402e8a5514f9c43Af0322b665F" => Ok(U8::from(2)),
                // Unichain Core StableSwap factory
                "0x604388Bb1159AFd21eB5191cE22b4DeCdEE2Ae22" => Ok(U8::from(1)),
                _ => Err(EncodingError::FatalError(format!(
                    "Unsupported curve factory address: {factory_address}"
                ))),
            },
        }
    }

    // Some curve pools support both ETH and WETH as tokens.
    // They do the wrapping/unwrapping inside the pool
    fn normalize_token(&self, token: Address, coins: &[Address]) -> Result<Address, EncodingError> {
        let eth_address = Address::from_slice(&ROUTER_ETH_ADDRESS);
        let wrapped_native_token_address = bytes_to_address(&self.wrapped_native_token_address)?;
        if token == eth_address && !coins.contains(&token) {
            Ok(wrapped_native_token_address)
        } else if token == wrapped_native_token_address && !coins.contains(&token) {
            Ok(eth_address)
        } else {
            Ok(token)
        }
    }

    fn get_coin_indexes(
        &self,
        swap: &Swap,
        token_in: Address,
        token_out: Address,
    ) -> Result<(U8, U8), EncodingError> {
        let coins_bytes = get_static_attribute(swap, "coins")?;
        let coins: Vec<Address> = from_str(std::str::from_utf8(&coins_bytes)?)?;

        let token_in = self.normalize_token(token_in, &coins)?;
        let token_out = self.normalize_token(token_out, &coins)?;

        let i = coins
            .iter()
            .position(|&addr| addr == token_in)
            .ok_or(EncodingError::FatalError(format!(
                "Token in address {token_in} not found in curve pool coins"
            )))?;
        let j = coins
            .iter()
            .position(|&addr| addr == token_out)
            .ok_or(EncodingError::FatalError(format!(
                "Token in address {token_out} not found in curve pool coins"
            )))?;
        Ok((U8::from(i), U8::from(j)))
    }
}

impl SwapEncoder for CurveSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self {
            executor_address,
            native_token_address: chain.native_token().address,
            wrapped_native_token_address: chain.wrapped_native_token().address,
        })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let eth_address = Address::from_slice(&ROUTER_ETH_ADDRESS);
        let token_in = if *swap.token_in().address == self.native_token_address {
            eth_address
        } else {
            bytes_to_address(&swap.token_in().address)?
        };
        let token_out = if *swap.token_out().address == self.native_token_address {
            eth_address
        } else {
            bytes_to_address(&swap.token_out().address)?
        };

        let component_address = Address::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid curve pool address".to_string()))?;

        let factory_bytes = get_static_attribute(swap, "factory")?.to_vec();
        // the conversion to Address is necessary to checksum the address
        let factory_address =
            Address::from_str(std::str::from_utf8(&factory_bytes).map_err(|_| {
                EncodingError::FatalError(
                    "Failed to convert curve factory address to string".to_string(),
                )
            })?)
            .map_err(|_| EncodingError::FatalError("Invalid curve factory address".to_string()))?;

        let pool_address = Address::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid curve pool address".to_string()))?;
        let pool_type =
            self.get_pool_type(&pool_address.to_string(), &factory_address.to_string())?;

        let (i, j) = self.get_coin_indexes(swap, token_in, token_out)?;

        let args = (
            token_in,
            token_out,
            component_address,
            pool_type.to_be_bytes::<1>(),
            i.to_be_bytes::<1>(),
            j.to_be_bytes::<1>(),
        );

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
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::swap_encoder::curve::CurveSwapEncoder, models::default_token};

    fn curve_config() -> Option<HashMap<String, String>> {
        None
    }

    #[rstest]
    #[case(
    "0x5b22307838363533373733363730353435313665313730313463636465643165376438313465646339636534222c22307861353538386637636466353630383131373130613264383264336339633939373639646231646362225d",
    "0x865377367054516e17014CcdED1e7d814EDC9ce4",
    "0xA5588F7cdf560811710A2D82D3C9c99769DB1Dcb",
    0,
    1
    )]
    #[case(
    "0x5b22307836623137353437346538393039346334346461393862393534656564656163343935323731643066222c22307861306238363939316336323138623336633164313964346132653965623063653336303665623438222c22307864616331376639353864326565353233613232303632303639393435393763313364383331656337222c22307835376162316563323864313239373037303532646634646634313864353861326434366435663531225d",
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    "0x57Ab1ec28D129707052df4dF418D58a2D46d5f51",
    1,
    3
    )]
    #[case(
    "0x5b22307864616331376639353864326565353233613232303632303639393435393763313364383331656337222c22307832323630666163356535353432613737336161343466626366656466376331393362633263353939222c22307863303261616133396232323366653864306130653563346632376561643930383363373536636332225d",
    "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
    "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
    2,
    1
    )]
    #[case(
    "0x5b22307861306238363939316336323138623336633164313964346132653965623063653336303665623438222c22307832323630666163356535353432613737336161343466626366656466376331393362633263353939222c22307865656565656565656565656565656565656565656565656565656565656565656565656565656565225d",
    "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    2,
    0
    )]
    // Pool that holds ETH but coin is WETH
    #[case(
    "0x5b22307861306238363939316336323138623336633164313964346132653965623063653336303665623438222c22307832323630666163356535353432613737336161343466626366656466376331393362633263353939222c22307865656565656565656565656565656565656565656565656565656565656565656565656565656565225d",
    "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE",
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    2,
    0
    )]
    // Pool that holds ETH but coin is WETH
    #[case(
    "0x5b22307861306238363939316336323138623336633164313964346132653965623063653336303665623438222c22307832323630666163356535353432613737336161343466626366656466376331393362633263353939222c22307865656565656565656565656565656565656565656565656565656565656565656565656565656565225d",
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE",
    0,
    2
    )]
    fn test_curve_get_coin_indexes(
        #[case] coins: &str,
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] expected_i: u64,
        #[case] expected_j: u64,
    ) {
        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert("coins".into(), Bytes::from_str(coins).unwrap());
        let swap = Swap::new(
            ProtocolComponent {
                id: "pool-id".into(),
                protocol_system: String::from("vm:curve"),
                static_attributes,
                ..Default::default()
            },
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        );

        let encoder =
            CurveSwapEncoder::new(Bytes::default(), Chain::Ethereum, curve_config()).unwrap();
        let (i, j) = encoder
            .get_coin_indexes(
                &swap,
                Address::from_str(token_in).unwrap(),
                Address::from_str(token_out).unwrap(),
            )
            .unwrap();
        assert_eq!(i, U8::from(expected_i));
        assert_eq!(j, U8::from(expected_j));
    }

    #[test]
    fn test_curve_encode_tripool() {
        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert(
            "factory".into(),
            Bytes::from(
                "0x0000000000000000000000000000000000000000"
                    .as_bytes()
                    .to_vec(),
            ),
        );
        static_attributes.insert("coins".into(), Bytes::from_str("0x5b22307836623137353437346538393039346334346461393862393534656564656163343935323731643066222c22307861306238363939316336323138623336633164313964346132653965623063653336303665623438222c22307864616331376639353864326565353233613232303632303639393435393763313364383331656337225d").unwrap());
        let curve_tri_pool = ProtocolComponent {
            id: String::from("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
            protocol_system: String::from("vm:curve"),
            static_attributes,
            ..Default::default()
        };
        let token_in = Bytes::from("0x6B175474E89094C44Da98b954EedeAC495271d0F");
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let swap = Swap::new(
            curve_tri_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoding_context = EncodingContext {
            router_address: None,
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = CurveSwapEncoder::new(
            Bytes::from("0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f"),
            Chain::Ethereum,
            curve_config(),
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // token in
                "6b175474e89094c44da98b954eedeac495271d0f",
                // token out
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                // pool address
                "bebc44782c7db0a1a60cb6fe97d0b483032ff1c7",
                // pool type 1
                "01",
                // i index
                "00",
                // j index
                "01",
            ))
        );
    }

    #[test]
    fn test_curve_encode_factory() {
        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert(
            "factory".into(),
            Bytes::from(
                "0x6A8cbed756804B16E05E741eDaBd5cB544AE21bf"
                    .as_bytes()
                    .to_vec(),
            ),
        );
        static_attributes.insert("coins".into(), Bytes::from_str("0x5b22307834633965646435383532636439303566303836633735396538333833653039626666316536386233222c22307861306238363939316336323138623336633164313964346132653965623063653336303665623438225d").unwrap());
        let curve_pool = ProtocolComponent {
            id: String::from("0x02950460E2b9529D0E00284A5fA2d7bDF3fA4d72"),
            protocol_system: String::from("vm:curve"),
            static_attributes,
            ..Default::default()
        };
        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3");
        let swap = Swap::new(
            curve_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: None,
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = CurveSwapEncoder::new(
            Bytes::from("0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f"),
            Chain::Ethereum,
            curve_config(),
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // token in
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                // token out
                "4c9edd5852cd905f086c759e8383e09bff1e68b3",
                // pool address
                "02950460e2b9529d0e00284a5fa2d7bdf3fa4d72",
                // pool type 1
                "01",
                // i index
                "01",
                // j index
                "00",
            ))
        );
    }
    #[test]
    fn test_curve_encode_st_eth() {
        // This test is for the stETH pool, which is a special case in Curve
        // where the token in is ETH but not as the zero address.
        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert(
            "factory".into(),
            Bytes::from(
                "0x0000000000000000000000000000000000000000"
                    .as_bytes()
                    .to_vec(),
            ),
        );
        static_attributes.insert("coins".into(), Bytes::from_str("0x5b22307865656565656565656565656565656565656565656565656565656565656565656565656565656565222c22307861653761623936353230646533613138653565313131623565616162303935333132643766653834225d").unwrap());
        let curve_pool = ProtocolComponent {
            id: String::from("0xDC24316b9AE028F1497c275EB9192a3Ea0f67022"),
            protocol_system: String::from("vm:curve"),
            static_attributes,
            ..Default::default()
        };
        let token_in = Bytes::from("0x0000000000000000000000000000000000000000");
        let token_out = Bytes::from("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84");
        let swap = Swap::new(
            curve_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: None,
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = CurveSwapEncoder::new(
            Bytes::from("0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // token in
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                // token out
                "ae7ab96520de3a18e5e111b5eaab095312d7fe84",
                // pool address
                "dc24316b9ae028f1497c275eb9192a3ea0f67022",
                // pool type 1
                "01",
                // i index
                "00",
                // j index
                "01",
            ))
        );
    }
}
