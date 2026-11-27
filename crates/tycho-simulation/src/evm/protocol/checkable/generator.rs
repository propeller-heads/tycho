//! How to generate states for the property checks. A state that can be generated implements
//! [`StateCases`], which describes its cases as a [`Domain`]: the corner cases a fixed sweep checks
//! every time, and random ones for the generated run. The domains here are shared by every state:
//! unsigned integers, tokens, and sets of another domain's values.

use std::fmt::Debug;

use alloy::primitives::{Address, Uint};
use itertools::{iproduct, Itertools};
pub use proptest;
use proptest::prelude::*;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use super::CheckableState;

/// How to obtain states of this type for the checks: the corner cases and random ones.
pub trait StateCases: ProtocolSim + CheckableState + Clone {
    /// The tokens a state trades, at least two: an array where every state has the same number.
    type Tokens: AsRef<[Token]> + Clone + Debug + 'static;

    /// Valid states and the tokens they trade.
    fn cases() -> impl Domain<Value = Case<Self, Self::Tokens>>;
}

/// One generated case: a state and the tokens it trades.
#[derive(Debug, Clone)]
pub struct Case<S, T> {
    /// The state to check.
    pub state: S,
    /// The tokens it trades, the ones the checks quote between.
    pub tokens: T,
}

/// The values a generator draws from: its corners, the boundary values a fixed sweep tests every
/// time, and random values from the whole domain for the generated run.
pub trait Domain {
    type Value: Clone + Debug + 'static;

    /// The boundary values, where integer maths and float conversions break.
    fn corners(&self) -> Vec<Self::Value>;

    /// Values from the whole domain. A domain made of parts draws each part from that part's
    /// [`corner_or_random`](DomainExt::corner_or_random), so one part can sit at a corner while
    /// the others vary.
    fn random(&self) -> impl Strategy<Value = Self::Value>;
}

/// What every [`Domain`] provides, built on its corners and random values.
pub trait DomainExt: Domain {
    /// A corner one time in ten, otherwise a [`random`](Domain::random) value.
    fn corner_or_random(&self) -> impl Strategy<Value = Self::Value> {
        prop_oneof![
            1 => prop::sample::select(self.corners()),
            9 => self.random(),
        ]
    }

    /// The domain with `f` applied to every value, corners and random ones alike.
    fn map<F, U>(self, f: F) -> Map<Self, F>
    where
        Self: Sized,
        F: Fn(Self::Value) -> U,
        U: Clone + Debug + 'static,
    {
        Map { domain: self, f }
    }
}

impl<D: Domain + ?Sized> DomainExt for D {}

/// A domain with a function applied to its values, from [`DomainExt::map`].
#[derive(Debug, Clone)]
pub struct Map<D, F> {
    domain: D,
    f: F,
}

impl<D, F, U> Domain for Map<D, F>
where
    D: Domain,
    F: Fn(D::Value) -> U,
    U: Clone + Debug + 'static,
{
    type Value = U;

    fn corners(&self) -> Vec<U> {
        self.domain
            .corners()
            .into_iter()
            .map(&self.f)
            .collect()
    }

    fn random(&self) -> impl Strategy<Value = U> {
        self.domain.random().prop_map(&self.f)
    }
}

/// Implements [`Domain`] for a tuple of domains, each named with its index.
macro_rules! tuple_domain {
    ($($name:ident: $index:tt),+) => {
        /// Independent domains. The corners are every combination of theirs.
        impl<$($name: Domain),+> Domain for ($($name,)+) {
            type Value = ($($name::Value,)+);

            fn corners(&self) -> Vec<Self::Value> {
                iproduct!($(self.$index.corners()),+).collect()
            }

            fn random(&self) -> impl Strategy<Value = Self::Value> {
                ($(self.$index.corner_or_random(),)+)
            }
        }
    };
}

tuple_domain!(A: 0, B: 1);
tuple_domain!(A: 0, B: 1, C: 2);
tuple_domain!(A: 0, B: 1, C: 2, D: 3);

/// Every value of an unsigned integer type, with the two smallest and the two largest as corners;
/// random values have every bit length from 0 to `BITS` equally likely, so small and large values
/// both occur.
#[derive(Debug, Clone, Copy)]
pub struct AnyUint<const BITS: usize, const LIMBS: usize>;

impl<const BITS: usize, const LIMBS: usize> Domain for AnyUint<BITS, LIMBS> {
    type Value = Uint<BITS, LIMBS>;

    fn corners(&self) -> Vec<Self::Value> {
        vec![Uint::ZERO, Uint::ONE, Uint::MAX - Uint::ONE, Uint::MAX]
    }

    fn random(&self) -> impl Strategy<Value = Self::Value> {
        (0..=BITS, any::<Uint<BITS, LIMBS>>()).prop_map(|(bits, value)| {
            if bits == 0 {
                Uint::ZERO
            } else {
                // The top `bits` bits of `value`, with the highest set so the length is `bits`.
                (value >> (BITS - bits)) | (Uint::ONE << (bits - 1))
            }
        })
    }
}

/// Sets of values from a domain. The corner is one set of all the domain's corners, so a sweep
/// checks every corner together; random sets hold up to as many values as the domain has corners,
/// each a corner of the domain one time in ten.
#[derive(Debug, Clone, Copy)]
pub struct SetOf<D>(
    /// The domain the values come from.
    pub D,
);

impl<D: Domain> Domain for SetOf<D> {
    type Value = Vec<D::Value>;

    fn corners(&self) -> Vec<Self::Value> {
        vec![self.0.corners()]
    }

    fn random(&self) -> impl Strategy<Value = Self::Value> {
        prop::collection::vec(self.0.corner_or_random(), 0..=self.0.corners().len())
    }
}

/// `N` tokens with distinct addresses, sorted by address, on Ethereum. The corners are every
/// combination of the decimals 0, 18, 39, 78 and 255, at the addresses 0x…01, 0x…02 and so on;
/// random tokens have random addresses and decimals.
#[derive(Debug, Clone, Copy)]
pub struct AnyTokens<const N: usize>;

impl<const N: usize> Domain for AnyTokens<N> {
    type Value = [Token; N];

    fn corners(&self) -> Vec<[Token; N]> {
        let addresses: [Address; N] = std::array::from_fn(|i| {
            Address::with_last_byte(u8::try_from(i + 1).expect("at most 255 tokens"))
        });
        (0..N)
            .map(|_| Decimals.corners())
            .multi_cartesian_product()
            .map(|decimals| std::array::from_fn(|i| token(i, addresses[i], decimals[i])))
            .collect()
    }

    fn random(&self) -> impl Strategy<Value = [Token; N]> {
        (
            prop::collection::btree_set(any::<[u8; 20]>().prop_map(Address::from), N),
            prop::array::uniform::<_, N>(Decimals.corner_or_random()),
        )
            .prop_map(|(addresses, decimals)| {
                let addresses = Vec::from_iter(addresses);
                std::array::from_fn(|i| token(i, addresses[i], decimals[i]))
            })
    }
}

/// The token at `index` of a case, on Ethereum, named T0, T1 and so on.
fn token(index: usize, address: Address, decimals: u32) -> Token {
    Token::new(
        &Bytes::from(address.as_slice()),
        &format!("T{index}"),
        decimals,
        0,
        &[Some(100_000)],
        Chain::Ethereum,
        100,
    )
}

/// Token decimals, which an ERC20 reports as a `uint8`. The corners are 0, the common 18, the
/// first decimals whose `10^decimals` leaves a `u128` (39) and a `U256` (78), and the largest
/// `uint8` (255); random values are the common 6, 8 or 18 or any `uint8`, equally often.
#[derive(Debug, Clone, Copy)]
struct Decimals;

impl Domain for Decimals {
    type Value = u32;

    fn corners(&self) -> Vec<u32> {
        vec![0, 18, 39, 78, 255]
    }

    fn random(&self) -> impl Strategy<Value = u32> {
        prop_oneof![prop::sample::select(vec![6u32, 8, 18]), 0u32..=255,]
    }
}
