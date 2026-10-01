//! Camelot V3 pool discovery.
//!
//! Camelot V3 on Arbitrum One is an Algebra V1.9 deployment. `AlgebraFactory.createPool` does
//! two things that matter for indexing:
//!
//! 1. It deploys a `DataStorageOperator` with `CREATE`. That contract holds the pool's oracle
//!    timepoints and adaptive fee configuration; the pool calls it on every swap (`write`,
//!    `getFees`, `calculateVolumePerLiquidity`). Its address appears in no event; it is only an
//!    immutable of the pool.
//! 2. It calls `AlgebraPoolDeployer.deploy`, which deploys the pool with `CREATE2` and then the
//!    factory emits `Pool(token0, token1, pool)`.
//!
//! Simulation runs in an empty VM, so a pool is only usable when the pool, its operator and the
//! factory (read for `vaultAddress` on every swap paying a community fee) are all indexed.
use anyhow::{bail, Result};
use substreams_ethereum::{
    pb::eth::v2::{Call, CallType, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

use crate::abi::factory::events::Pool as PoolCreated;

/// Tycho protocol type of every Camelot V3 pool component.
pub const PROTOCOL_TYPE: &str = "camelot_v3_pool";

/// Static attribute holding the pool's `DataStorageOperator` address as raw bytes.
pub const DATA_STORAGE_OPERATOR_ATTRIBUTE: &str = "data_storage_operator";

/// Reserved static attribute: a pool is only refreshed when a transaction carries an
/// `update_marker` for it, which `map_protocol_changes` emits when the pool's own or its
/// operator's storage changed. Without it every factory storage change, such as each
/// `createPool`, would refresh every Camelot pool.
pub const MANUAL_UPDATES_ATTRIBUTE: &str = "manual_updates";
pub const MANUAL_UPDATES_VALUE: [u8; 1] = [1];

/// State attribute holding the pool's current `activeIncentive` address.
///
/// A non-zero value means the pool calls that virtual pool on every swap. Such a contract is
/// not indexed by this package, so consumers must not quote pools whose value is non-zero.
/// The value is read from storage rather than from `Incentive` events: `setIncentive` emits
/// one, but a swap that finds the incentive gone clears the field without any event.
pub const ACTIVE_INCENTIVE_ATTRIBUTE: &str = "active_incentive";

/// Storage slot 4 of an `AlgebraPool`, which packs `liquidityCooldown` (bytes 0..4),
/// `activeIncentive` (bytes 4..24) and `tickSpacing` (bytes 24..27), counted from the low end.
pub const ACTIVE_INCENTIVE_SLOT: [u8; 32] = {
    let mut slot = [0u8; 32];
    slot[31] = 4;
    slot
};

/// Store key of the component a pool or operator contract belongs to, by the contract's
/// `0x`-prefixed lowercase hex address.
pub fn contract_key(address_hex: &str) -> String {
    format!("contract:{address_hex}")
}

/// Returns a component for every pool the factory created in `tx`, in call order.
///
/// Errors when a factory call emitted a `Pool` event but did not create exactly one contract
/// itself: the created contract is the pool's `DataStorageOperator`, and a pool indexed without
/// it can never be simulated, so the shape is enforced rather than guessed around.
pub fn pools_created(factory: &[u8], tx: &TransactionTrace) -> Result<Vec<ProtocolComponent>> {
    let mut components = Vec::new();
    for call in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted && call.address == factory)
    {
        let events: Vec<PoolCreated> = call
            .logs
            .iter()
            .filter_map(PoolCreated::match_and_decode)
            .collect();
        let event = match events.as_slice() {
            [] => continue,
            [event] => event,
            _ => bail!(
                "factory call {} in tx 0x{} emitted {} Pool events, expected one per createPool",
                call.index,
                hex::encode(&tx.hash),
                events.len()
            ),
        };
        let operator = data_storage_operator(call, tx)?;
        components.push(
            ProtocolComponent::new(&event.pool.to_hex())
                .with_tokens(&[event.token0.as_slice(), event.token1.as_slice()])
                .with_contracts(&[event.pool.as_slice(), operator.as_slice(), factory])
                .with_attributes(&[
                    (DATA_STORAGE_OPERATOR_ATTRIBUTE, operator.as_slice()),
                    (MANUAL_UPDATES_ATTRIBUTE, MANUAL_UPDATES_VALUE.as_slice()),
                ])
                .as_swap_type(PROTOCOL_TYPE, ImplementationType::Vm),
        );
    }
    Ok(components)
}

/// The `activeIncentive` address held in a value of [`ACTIVE_INCENTIVE_SLOT`].
///
/// Firehose delivers storage keys and values as full 32-byte words, which is also what the
/// exact key comparison against [`ACTIVE_INCENTIVE_SLOT`] relies on. A value of any other
/// length therefore means that assumption broke and is an error, not something to pad.
pub fn active_incentive_from_slot(value: &[u8]) -> Result<Vec<u8>> {
    if value.len() != 32 {
        bail!("activeIncentive slot value of {} bytes is not a 32-byte word", value.len());
    }
    Ok(value[8..28].to_vec())
}

/// The address of the single contract `create_pool` deployed directly with `CREATE`.
///
/// The pool itself is deployed by the pool deployer, one call level deeper, so it never matches.
fn data_storage_operator(create_pool: &Call, tx: &TransactionTrace) -> Result<Vec<u8>> {
    let mut created = tx.calls.iter().filter(|call| {
        call.parent_index == create_pool.index &&
            call.call_type() == CallType::Create &&
            !call.state_reverted
    });
    let Some(operator) = created.next() else {
        bail!(
            "factory call {} in tx 0x{} emitted a Pool event but created no DataStorageOperator",
            create_pool.index,
            hex::encode(&tx.hash)
        )
    };
    if let Some(extra) = created.next() {
        bail!(
            "factory call {} in tx 0x{} created more than one contract (0x{} and 0x{}), cannot \
             tell which is the DataStorageOperator",
            create_pool.index,
            hex::encode(&tx.hash),
            hex::encode(&operator.address),
            hex::encode(&extra.address)
        )
    }
    Ok(operator.address.clone())
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::Log;

    use super::*;

    const FACTORY: [u8; 20] = [0xfa; 20];
    const DEPLOYER: [u8; 20] = [0xde; 20];
    const OPERATOR: [u8; 20] = [0x0b; 20];
    const POOL: [u8; 20] = [0xb0; 20];
    const TOKEN0: [u8; 20] = [0x01; 20];
    const TOKEN1: [u8; 20] = [0x02; 20];

    fn padded(address: &[u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(address);
        word
    }

    /// The factory's `Pool(token0, token1, pool)` log.
    fn pool_log(emitter: &[u8; 20]) -> Log {
        let topic = hex::decode("91ccaa7a278130b65168c3a0c8d3bcae84cf5e43704342bd3ec0b59e59c036db")
            .unwrap();
        Log {
            address: emitter.to_vec(),
            topics: vec![topic, padded(&TOKEN0), padded(&TOKEN1)],
            data: padded(&POOL),
            ..Default::default()
        }
    }

    fn call(index: u32, parent_index: u32, call_type: CallType, address: &[u8; 20]) -> Call {
        Call {
            index,
            parent_index,
            call_type: call_type as i32,
            address: address.to_vec(),
            ..Default::default()
        }
    }

    /// The call tree of a real `createPool`: the factory frame emits `Pool`, creates the
    /// operator directly, and has the deployer create the pool one level deeper.
    fn create_pool_tx() -> TransactionTrace {
        let mut factory_call = call(1, 0, CallType::Call, &FACTORY);
        factory_call.logs = vec![pool_log(&FACTORY)];
        TransactionTrace {
            hash: vec![0xaa; 32],
            calls: vec![
                factory_call,
                call(2, 1, CallType::Create, &OPERATOR),
                call(3, 1, CallType::Call, &DEPLOYER),
                call(4, 3, CallType::Create, &POOL),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn discovers_pool_with_operator_and_factory() {
        let components = pools_created(&FACTORY, &create_pool_tx()).unwrap();

        assert_eq!(components.len(), 1);
        let component = &components[0];
        assert_eq!(component.id, POOL.to_hex());
        assert_eq!(component.tokens, vec![TOKEN0.to_vec(), TOKEN1.to_vec()]);
        assert_eq!(component.contracts, vec![POOL.to_vec(), OPERATOR.to_vec(), FACTORY.to_vec()]);
        assert_eq!(
            component.get_attribute_value(DATA_STORAGE_OPERATOR_ATTRIBUTE),
            Some(OPERATOR.to_vec())
        );
        assert_eq!(
            component.get_attribute_value(MANUAL_UPDATES_ATTRIBUTE),
            Some(MANUAL_UPDATES_VALUE.to_vec())
        );
        let protocol_type = component
            .protocol_type
            .as_ref()
            .unwrap();
        assert_eq!(protocol_type.name, PROTOCOL_TYPE);
        assert_eq!(protocol_type.implementation_type, ImplementationType::Vm as i32);
    }

    #[test]
    fn ignores_reverted_factory_call() {
        let mut tx = create_pool_tx();
        tx.calls[0].state_reverted = true;

        assert!(pools_created(&FACTORY, &tx)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ignores_pool_log_from_other_contract() {
        let mut tx = create_pool_tx();
        tx.calls[0].address = DEPLOYER.to_vec();
        tx.calls[0].logs = vec![pool_log(&DEPLOYER)];

        assert!(pools_created(&FACTORY, &tx)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn errors_without_operator_creation() {
        let mut tx = create_pool_tx();
        tx.calls.remove(1);

        assert!(pools_created(&FACTORY, &tx).is_err());
    }

    #[test]
    fn reads_active_incentive_from_slot_word() {
        // liquidityCooldown = 0, activeIncentive = 0x0c.., tickSpacing = 60
        let mut word = [0u8; 32];
        word[8..28].copy_from_slice(&[0x0c; 20]);
        word[7] = 60;
        assert_eq!(active_incentive_from_slot(&word).unwrap(), vec![0x0c; 20]);
    }

    #[test]
    fn rejects_short_slot_value() {
        assert!(active_incentive_from_slot(&[60]).is_err());
        assert!(active_incentive_from_slot(&[]).is_err());
        assert!(active_incentive_from_slot(&[0u8; 31]).is_err());
    }

    #[test]
    fn rejects_oversized_slot_value() {
        assert!(active_incentive_from_slot(&[1u8; 33]).is_err());
    }

    #[test]
    fn errors_with_ambiguous_operator_creation() {
        let mut tx = create_pool_tx();
        tx.calls
            .push(call(5, 1, CallType::Create, &[0x0c; 20]));

        assert!(pools_created(&FACTORY, &tx).is_err());
    }
}
