//! What every PancakeSwap Infinity substreams package shares, whatever the pool type: the
//! `PoolKey.parameters` hook bitmap, the dynamic fee flag, the `pools` mapping slot arithmetic,
//! the protocol fee split, the deployment `params` and the `Transaction` conversions.
//!
//! This crate exports no `#[substreams::handlers]` function. A handler is a `#[no_mangle]`
//! symbol, and a wasm keeps every such symbol from every crate it links, so a crate holding
//! handlers cannot be a dependency of two packages at once. Handlers, protobufs, ABIs, event maps
//! and stores stay in their package.
//!
//! Solidity references are paths in `pancakeswap/infinity-core` at commit `7c04695f`
//! (<https://github.com/pancakeswap/infinity-core/tree/7c04695f>).

pub mod parameters;
pub mod params;

/// Conversions between a package's protobuf `Transaction` and the substreams and Tycho ones.
///
/// Each package generates its own `Transaction` message, so the impls have to live in that
/// package: Rust allows a trait impl only where the trait or the type is defined. The message
/// must carry `hash`, `from`, `to` as `bytes` and `index` as `uint64`, which every Infinity
/// package's `Transaction` does.
#[macro_export]
macro_rules! transaction_conversions {
    ($transaction:ty) => {
        impl From<::substreams_ethereum::pb::eth::v2::TransactionTrace> for $transaction {
            fn from(value: ::substreams_ethereum::pb::eth::v2::TransactionTrace) -> Self {
                Self { hash: value.hash, from: value.from, to: value.to, index: value.index.into() }
            }
        }

        impl From<&::substreams_ethereum::pb::eth::v2::TransactionTrace> for $transaction {
            fn from(value: &::substreams_ethereum::pb::eth::v2::TransactionTrace) -> Self {
                Self {
                    hash: value.hash.clone(),
                    from: value.from.clone(),
                    to: value.to.clone(),
                    index: value.index.into(),
                }
            }
        }

        impl From<$transaction> for ::tycho_substreams::prelude::Transaction {
            fn from(value: $transaction) -> Self {
                Self { hash: value.hash, from: value.from, to: value.to, index: value.index }
            }
        }

        impl From<&$transaction> for ::tycho_substreams::prelude::Transaction {
            fn from(value: &$transaction) -> Self {
                Self {
                    hash: value.hash.clone(),
                    from: value.from.clone(),
                    to: value.to.clone(),
                    index: value.index,
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::TransactionTrace;

    /// The shape of every package's protobuf `Transaction`.
    #[derive(Debug, PartialEq)]
    struct Transaction {
        hash: Vec<u8>,
        from: Vec<u8>,
        to: Vec<u8>,
        index: u64,
    }

    transaction_conversions!(Transaction);

    #[test]
    fn trace_round_trips_to_the_tycho_transaction() {
        let trace = TransactionTrace {
            hash: vec![1, 2],
            from: vec![3],
            to: vec![4],
            index: 7,
            ..Default::default()
        };

        let owned = Transaction::from(trace.clone());
        assert_eq!(owned, Transaction::from(&trace));
        assert_eq!(owned, Transaction { hash: vec![1, 2], from: vec![3], to: vec![4], index: 7 });

        let tycho = tycho_substreams::prelude::Transaction::from(&owned);
        assert_eq!(tycho, tycho_substreams::prelude::Transaction::from(owned));
        assert_eq!(
            (tycho.hash, tycho.from, tycho.to, tycho.index),
            (vec![1, 2], vec![3], vec![4], 7)
        );
    }
}
