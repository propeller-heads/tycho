//! Kuru (Monad CLOB) book rules: how each market event moves the price levels.
//!
//! One implementation for both consumers: the `monad-kuru` substreams (stores, wasm) and
//! tycho-simulation's resync/replay (`LevelBook`). Levels are sums of order sizes per price
//! (sizePrecision units); prices are `uint32` in pricePrecision units.
#![no_std]
extern crate alloc;

use alloc::{collections::BTreeMap, vec::Vec};

/// Market events, already decoded (ABI decoding is per consumer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// OrderCreated, FlipOrderCreated, FlippedOrderCreated
    Created {
        id: u64,
        price: u32,
        size: u128,
        is_buy: bool,
    },
    OrderCanceled {
        id: u64,
        price: u32,
        size: u128,
        is_buy: bool,
    },
    /// Carries only the new size of `id` (a flip order's twin).
    FlipOrderUpdated {
        id: u64,
        size: u128,
    },
    /// `taker_buy` is `Trade.isBuy`; `price_1e18` is `Trade.price`. `id` 0 = vault fill.
    Trade {
        id: u64,
        taker_buy: bool,
        price_1e18: u128,
        updated_size: u128,
        filled: u128,
    },
    MarketState {
        active: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delta {
    pub is_buy: bool,
    pub price: u32,
    pub add: u128,
    pub sub: u128,
}

/// A live order as the rules need it: (price, is_buy, size).
pub type OrderRef = (u32, bool, u128);

const VPP: u128 = 1_000_000_000_000_000_000;

impl Event {
    /// The order whose size this event sets, and the size (0 = gone).
    pub fn order_size(&self) -> Option<(u64, u128)> {
        match *self {
            Event::Created { id, size, .. } => Some((id, size)),
            Event::OrderCanceled { id, .. } => Some((id, 0)),
            Event::FlipOrderUpdated { id, size } => Some((id, size)),
            Event::Trade { id, updated_size, .. } if id != 0 => Some((id, updated_size)),
            _ => None,
        }
    }

    /// Level delta of this event. `order` = the order the event names, as it stood just before it:
    /// required for `FlipOrderUpdated` (it carries only the new size), used for `Trade` when known.
    pub fn delta(
        &self,
        price_precision: u128,
        order: Option<OrderRef>,
    ) -> Result<Option<Delta>, &'static str> {
        Ok(match *self {
            Event::Created { price, size, is_buy, .. } => {
                Some(Delta { is_buy, price, add: size, sub: 0 })
            }
            Event::OrderCanceled { price, size, is_buy, .. } => {
                Some(Delta { is_buy, price, add: 0, sub: size })
            }
            Event::FlipOrderUpdated { size, .. } => {
                let (price, is_buy, old) = order.ok_or("FlipOrderUpdated on an unknown order")?;
                Some(if size >= old {
                    Delta { is_buy, price, add: size - old, sub: 0 }
                } else {
                    Delta { is_buy, price, add: 0, sub: old - size }
                })
            }
            // Vault fills move vault storage, not levels.
            Event::Trade { id: 0, .. } => None,
            Event::Trade { taker_buy, price_1e18, filled, .. } => {
                // the maker order's own tick when known; else from `Trade.price` = tick * 1e18 /
                // pricePrecision, exact for the power-of-ten precisions the Router enforces
                let price = match order {
                    Some((price, _, _)) => price,
                    None => {
                        let scaled = price_1e18
                            .checked_mul(price_precision)
                            .ok_or("Trade price overflow")?;
                        if scaled % VPP != 0 {
                            return Err("Trade price is not on a tick");
                        }
                        u32::try_from(scaled / VPP).map_err(|_| "Trade price > uint32")?
                    }
                };
                Some(Delta { is_buy: !taker_buy, price, add: 0, sub: filled })
            }
            Event::MarketState { .. } => None,
        })
    }
}

/// Levels + live orders, replayed in memory (resync path; the substreams keeps the same in stores).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelBook {
    pub bids: BTreeMap<u32, u128>,
    pub asks: BTreeMap<u32, u128>,
    /// live order sizes, needed by `FlipOrderUpdated`; orders older than the snapshot must be
    /// seeded from `s_orders(id)`
    pub orders: BTreeMap<u64, OrderRef>,
    pub active: Option<bool>,
    pub price_precision: u128,
}

impl LevelBook {
    pub fn apply(&mut self, ev: &Event) -> Result<(), &'static str> {
        let order = match ev {
            Event::FlipOrderUpdated { id, .. } => Some(
                *self
                    .orders
                    .get(id)
                    .ok_or("FlipOrderUpdated on an unknown order")?,
            ),
            Event::Trade { id, .. } => self.orders.get(id).copied(),
            _ => None,
        };
        if let Some(d) = ev.delta(self.price_precision, order)? {
            let side = if d.is_buy { &mut self.bids } else { &mut self.asks };
            let cur = side.get(&d.price).copied().unwrap_or(0) + d.add;
            let new = cur
                .checked_sub(d.sub)
                .ok_or("level underflow")?;
            if new == 0 {
                side.remove(&d.price);
            } else {
                side.insert(d.price, new);
            }
        }
        match *ev {
            Event::Created { id, price, size, is_buy } => {
                self.orders
                    .insert(id, (price, is_buy, size));
            }
            Event::MarketState { active } => self.active = Some(active),
            _ => {
                if let Some((id, size)) = ev.order_size() {
                    if size == 0 {
                        self.orders.remove(&id);
                    } else if let Some(o) = self.orders.get_mut(&id) {
                        o.2 = size;
                    }
                }
            }
        }
        Ok(())
    }

    /// Ids `FlipOrderUpdated` names that no `Created` in `evs` introduces: seed these first.
    pub fn unseeded(evs: &[Event]) -> Vec<u64> {
        let mut born = alloc::collections::BTreeSet::new();
        let mut need = Vec::new();
        for ev in evs {
            match *ev {
                Event::Created { id, .. } => {
                    born.insert(id);
                }
                Event::FlipOrderUpdated { id, .. }
                    if !born.contains(&id) && !need.contains(&id) =>
                {
                    need.push(id)
                }
                _ => {}
            }
        }
        need
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_trade_flip_cancel() {
        let mut b = LevelBook { price_precision: VPP, ..Default::default() };
        b.apply(&Event::Created { id: 1, price: 5, size: 10, is_buy: false })
            .unwrap();
        b.apply(&Event::Trade {
            id: 1,
            taker_buy: true,
            price_1e18: 5,
            updated_size: 6,
            filled: 4,
        })
        .unwrap();
        assert_eq!(b.asks.get(&5), Some(&6));
        b.apply(&Event::FlipOrderUpdated { id: 1, size: 9 })
            .unwrap();
        assert_eq!(b.asks.get(&5), Some(&9));
        b.apply(&Event::Trade {
            id: 0,
            taker_buy: true,
            price_1e18: 7,
            updated_size: 0,
            filled: 3,
        })
        .unwrap();
        b.apply(&Event::OrderCanceled { id: 1, price: 5, size: 9, is_buy: false })
            .unwrap();
        assert!(b.asks.is_empty() && b.orders.is_empty());
        assert!(b
            .apply(&Event::FlipOrderUpdated { id: 2, size: 1 })
            .is_err());
        assert_eq!(
            LevelBook::unseeded(&[
                Event::FlipOrderUpdated { id: 3, size: 1 },
                Event::Created { id: 4, price: 1, size: 1, is_buy: true },
                Event::FlipOrderUpdated { id: 4, size: 2 }
            ]),
            [3]
        );
    }

    #[test]
    fn trade_prefers_the_order_tick() {
        // pp 3 does not divide 1e18: the price is only recoverable from the order itself
        let t =
            Event::Trade { id: 9, taker_buy: true, price_1e18: 333, updated_size: 0, filled: 2 };
        assert!(t.delta(3, None).is_err());
        assert_eq!(
            t.delta(3, Some((7, false, 2))).unwrap(),
            Some(Delta { is_buy: false, price: 7, add: 0, sub: 2 })
        );
    }

    #[test]
    fn trade_price_back_to_ticks() {
        // pp 1e8: Trade.price = price * 1e18 / 1e8
        let d = Event::Trade {
            id: 9,
            taker_buy: false,
            price_1e18: 2_890_900 * 10_000_000_000,
            updated_size: 0,
            filled: 1,
        }
        .delta(100_000_000, None)
        .unwrap()
        .unwrap();
        assert_eq!(d, Delta { is_buy: true, price: 2_890_900, add: 0, sub: 1 });
    }
}
