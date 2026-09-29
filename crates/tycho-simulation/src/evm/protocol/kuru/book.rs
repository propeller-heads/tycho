//! Chain-side views of a Kuru market: the `getL2Book()` / `getVaultParams()` /
//! `getMarketParams()` snapshot (resync) and the event application that keeps levels current
//! between snapshots via the shared `kuru_book` rules (the substreams module uses the same crate).
use std::collections::BTreeMap;

use alloy::{primitives::U256, sol, sol_types::SolEvent};
use kuru_book::{Event, LevelBook};
use tycho_common::Bytes;

use super::state::{KuruState, Vault};

sol! {
    struct Order {
        address ownerAddress;
        uint96 size;
        uint40 prev;
        uint40 next;
        uint40 flippedId;
        uint32 price;
        uint32 flippedPrice;
        bool isBuy;
    }
    interface IKuru {
        function getL2Book() external view returns (bytes memory);
        function getVaultParams() external view returns (address, uint256, uint96, uint256, uint96, uint96, uint96, uint96);
        function getMarketParams() external view returns (uint32, uint96, address, uint256, address, uint256, uint32, uint96, uint96, uint256, uint256);
        function marketState() external view returns (uint8);
        function s_orders(uint40 id) external view returns (address ownerAddress, uint96 size, uint40 prev, uint40 next, uint40 flippedId, uint32 price, uint32 flippedPrice, bool isBuy);
        function placeAndExecuteMarketBuy(uint96 quoteSize, uint256 minAmountOut, bool isMargin, bool isFillOrKill) external payable returns (uint256);
        function placeAndExecuteMarketSell(uint96 size, uint256 minAmountOut, bool isMargin, bool isFillOrKill) external payable returns (uint256);
    }
    event OrderCreated(uint40 orderId, address owner, uint96 size, uint32 price, bool isBuy);
    event FlipOrderCreated(uint40 orderId, uint40 flippedId, address owner, uint96 size, uint32 price, uint32 flippedPrice, bool isBuy);
    event FlippedOrderCreated(uint40 orderId, uint40 flippedId, address owner, uint96 size, uint32 price, uint32 flippedPrice, bool isBuy);
    event FlipOrderUpdated(uint40 orderId, uint96 size);
    event OrderCanceled(uint40 orderId, address owner, uint32 price, uint96 size, bool isBuy);
    event Trade(uint40 orderId, address makerAddress, bool isBuy, uint256 price, uint96 updatedSize, address takerAddress, address txOrigin, uint96 filledSize);
    event MarketStateUpdated(uint8 previousState, uint8 newState);
}

/// `getL2Book()` bytes: block number, (price, size) bids best-first, a 0 word, asks best-first.
pub fn parse_l2(data: &[u8]) -> Result<(u64, BTreeMap<u32, U256>, BTreeMap<u32, U256>), String> {
    if data.len() % 32 != 0 || data.len() < 64 {
        return Err(format!("L2 length {}", data.len()));
    }
    let w: Vec<U256> = data
        .chunks(32)
        .map(U256::from_be_slice)
        .collect();
    let block = u64::try_from(w[0]).map_err(|_| "L2 block")?;
    let (mut bids, mut asks) = (BTreeMap::new(), BTreeMap::new());
    let mut i = 1;
    let mut side = &mut bids;
    let mut on_bids = true;
    while i < w.len() {
        if on_bids && w[i].is_zero() {
            on_bids = false;
            side = &mut asks;
            i += 1;
            continue;
        }
        let price = u32::try_from(w[i]).map_err(|_| "L2 price")?;
        let size = *w.get(i + 1).ok_or("L2 truncated")?;
        side.insert(price, size);
        i += 2;
    }
    Ok((block, bids, asks))
}

/// State from the three view calls (and `marketState()`), all read at one block.
pub fn state_from_views(
    l2: &[u8],
    vault: &IKuru::getVaultParamsReturn,
    market: &IKuru::getMarketParamsReturn,
    market_state: u8,
) -> Result<KuruState, String> {
    let (_, bids, asks) = parse_l2(l2)?;
    let pow = |d: U256| U256::from(10u64).pow(d);
    Ok(KuruState {
        base: Bytes::from(market._2.to_vec()),
        quote: Bytes::from(market._4.to_vec()),
        price_precision: U256::from(market._0),
        size_precision: U256::from(market._1),
        base_mult: pow(market._3),
        quote_mult: pow(market._5),
        taker_fee_bps: market._9,
        maker_fee_bps: market._10,
        active: market_state == 0,
        bids,
        asks,
        vault: Vault {
            best_bid: vault._1,
            bid_partial: U256::from(vault._2),
            best_ask: vault._3,
            ask_partial: U256::from(vault._4),
            bid_size: U256::from(vault._5),
            ask_size: U256::from(vault._6),
            spread: U256::from(vault._7),
        },
    })
}

/// Decodes one market log into the shared book event (`kuru_book`); `None` = not a book event.
pub fn decode(topic0: [u8; 32], data: &[u8]) -> Result<Option<Event>, String> {
    let t0 = alloy::primitives::B256::from(topic0);
    let e = |x: alloy::sol_types::Error| x.to_string();
    let id = |x: alloy::primitives::Uint<40, 1>| x.to::<u64>();
    Ok(Some(if t0 == OrderCreated::SIGNATURE_HASH {
        let d = OrderCreated::abi_decode_data(data).map_err(e)?;
        Event::Created { id: id(d.0), price: d.3, size: d.2.to(), is_buy: d.4 }
    } else if t0 == FlipOrderCreated::SIGNATURE_HASH {
        let d = FlipOrderCreated::abi_decode_data(data).map_err(e)?;
        Event::Created { id: id(d.0), price: d.4, size: d.3.to(), is_buy: d.6 }
    } else if t0 == FlippedOrderCreated::SIGNATURE_HASH {
        let d = FlippedOrderCreated::abi_decode_data(data).map_err(e)?;
        Event::Created { id: id(d.0), price: d.4, size: d.3.to(), is_buy: d.6 }
    } else if t0 == OrderCanceled::SIGNATURE_HASH {
        let d = OrderCanceled::abi_decode_data(data).map_err(e)?;
        Event::OrderCanceled { id: id(d.0), price: d.2, size: d.3.to(), is_buy: d.4 }
    } else if t0 == FlipOrderUpdated::SIGNATURE_HASH {
        let d = FlipOrderUpdated::abi_decode_data(data).map_err(e)?;
        Event::FlipOrderUpdated { id: id(d.0), size: d.1.to() }
    } else if t0 == Trade::SIGNATURE_HASH {
        let d = Trade::abi_decode_data(data).map_err(e)?;
        let price_1e18 = u128::try_from(d.3).map_err(|_| "Trade price > u128")?;
        Event::Trade {
            id: id(d.0),
            taker_buy: d.2,
            price_1e18,
            updated_size: d.4.to(),
            filled: d.7.to(),
        }
    } else if t0 == MarketStateUpdated::SIGNATURE_HASH {
        let d = MarketStateUpdated::abi_decode_data(data).map_err(e)?;
        Event::MarketState { active: d.1 == 0 }
    } else {
        return Ok(None);
    }))
}

/// Levels in `KuruState` units.
pub fn levels(b: &BTreeMap<u32, u128>) -> BTreeMap<u32, U256> {
    b.iter()
        .map(|(p, s)| (*p, U256::from(*s)))
        .collect()
}

/// `LevelBook` seeded from a snapshot's levels.
pub fn level_book(s: &KuruState) -> Result<LevelBook, String> {
    let down = |b: &BTreeMap<u32, U256>| -> Result<BTreeMap<u32, u128>, String> {
        b.iter()
            .map(|(p, s)| Ok((*p, u128::try_from(*s).map_err(|_| "level > u128")?)))
            .collect()
    };
    Ok(LevelBook {
        bids: down(&s.bids)?,
        asks: down(&s.asks)?,
        price_precision: u128::try_from(s.price_precision).map_err(|_| "pricePrecision")?,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::Address, sol_types::SolValue};

    use super::*;

    #[test]
    fn l2_roundtrip() {
        let words: Vec<U256> = [9u64, 100, 5, 99, 6, 0, 101, 7]
            .iter()
            .map(|x| U256::from(*x))
            .collect();
        let data: Vec<u8> = words
            .iter()
            .flat_map(|w| w.to_be_bytes::<32>())
            .collect();
        let (b, bids, asks) = parse_l2(&data).unwrap();
        assert_eq!(b, 9);
        assert_eq!(bids.len(), 2);
        assert_eq!(asks.get(&101), Some(&U256::from(7)));
    }

    #[test]
    fn decode_trade() {
        let data =
            (7u64, Address::ZERO, true, U256::from(5), 6u64, Address::ZERO, Address::ZERO, 4u64)
                .abi_encode_params();
        assert_eq!(
            decode(Trade::SIGNATURE_HASH.0, &data).unwrap(),
            Some(Event::Trade {
                id: 7,
                taker_buy: true,
                price_1e18: 5,
                updated_size: 6,
                filled: 4
            })
        );
        assert_eq!(decode([0u8; 32], &[]).unwrap(), None);
    }
}
