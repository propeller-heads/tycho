use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Result};
use substreams::{prelude::*, scalar::BigInt};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::erc20::{events::Transfer, functions::BalanceOf},
    prelude::*,
};

use crate::{config::Config, modules::store_components::component_created};

/// Tracks component balances that are only observable as relative changes.
///
/// Every component follows the same rule: its balances are seeded via deterministic
/// eth_calls snapshotting the post-block state of its creation block, and deltas are
/// only tracked for blocks strictly after it (in-block activity is already inside
/// the seed).
///
/// - PSM component: DAI held by the PSM itself (sell-side inventory) and USDC held by its pocket
///   (buy-side inventory), from ERC20 Transfer logs.
/// - Wrapper component: seeded here; ongoing values are mirrored from the PSM's absolute balances
///   in `map_protocol_changes`, not tracked as deltas.
/// - Converter component: not handled here at all — its escrow balances are read as absolute values
///   from Vat storage changes in `map_protocol_changes`, bypassing delta aggregation.
#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    components_store: StoreGetInt64,
) -> Result<BlockBalanceDeltas, substreams::errors::Error> {
    let config = Config::parse(&params)?;
    let mut balance_deltas = vec![];

    // Balance changes may only reference components that exist in the current run:
    // a run starting after a component's creation block (e.g. a scoped integration
    // test window) must not emit changes for it.
    let psm_created = component_created(&components_store, &config.psm_component_id());

    if block.number == config.psm_creation_block {
        balance_deltas.extend(seed_deltas(
            &block,
            &config.psm_creation_tx,
            config.psm_component_id(),
            [
                (&config.dai, balance_of(&config.dai, &config.psm)),
                (&config.usdc, balance_of(&config.usdc, &config.pocket)),
            ],
        )?);
    }
    if block.number == config.wrapper_creation_block {
        balance_deltas.extend(seed_deltas(
            &block,
            &config.wrapper_creation_tx,
            config.wrapper_component_id(),
            [
                (&config.usds, balance_of(&config.dai, &config.psm)),
                (&config.usdc, balance_of(&config.usdc, &config.pocket)),
            ],
        )?);
    }
    for log_view in block.logs() {
        let log = log_view.log;

        if ![config.dai.as_slice(), config.usdc.as_slice()].contains(&log.address.as_slice()) {
            continue;
        }

        let Some(transfer) = Transfer::match_and_decode(log) else {
            continue;
        };

        balance_deltas.extend(deltas_for_transfer(
            &config,
            block.number,
            psm_created,
            &log.address,
            transfer,
            log.ordinal,
            &(log_view.receipt.transaction.into()),
        ));
    }

    // Downstream aggregation groups deltas by consecutive tx hash and the balance store
    // expects non-decreasing ordinals: sorting keeps the seed deltas (pushed above,
    // out of tx order) from splitting a transaction's group and silently dropping its
    // earlier balance changes.
    balance_deltas.sort_unstable_by_key(|delta| delta.ord);

    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Routes one ERC20 Transfer touching the PSM or its pocket to a balance delta.
#[allow(clippy::too_many_arguments)]
fn deltas_for_transfer(
    config: &Config,
    block_number: u64,
    psm_created: bool,
    token: &[u8],
    transfer: Transfer,
    ord: u64,
    tx: &Transaction,
) -> Vec<BalanceDelta> {
    let (from, to, value) = (transfer.from.as_slice(), transfer.to.as_slice(), transfer.value);
    // A self-transfer is a net zero change; emitting +v and -v at the same ordinal for
    // the same key would violate the balance store's strictly-increasing-ordinal rule.
    if from == to || value.is_zero() {
        return vec![];
    }
    // Deltas start strictly after the PSM's creation block (in-block activity is
    // already inside the seed) and only once the component is part of this run.
    if !psm_created || block_number <= config.psm_creation_block {
        return vec![];
    }

    let mut deltas = vec![];
    let mut push = |component_id: String, token: Address, delta: BigInt| {
        deltas.push(BalanceDelta {
            ord,
            tx: Some(tx.clone()),
            token: token.to_vec(),
            delta: delta.to_signed_bytes_be(),
            component_id: component_id.into_bytes(),
        });
    };

    if token == config.dai {
        if to == config.psm {
            push(config.psm_component_id(), config.dai, value.clone());
        } else if from == config.psm {
            push(config.psm_component_id(), config.dai, value.neg());
        }
    } else if token == config.usdc {
        if to == config.pocket {
            push(config.psm_component_id(), config.usdc, value);
        } else if from == config.pocket {
            push(config.psm_component_id(), config.usdc, value.neg());
        }
    }

    deltas
}

/// Post-block `balanceOf` snapshot via a deterministic eth_call. A failed call means
/// a provider issue: fail the module loudly rather than seed a wrong balance.
fn balance_of(token: &Address, owner: &Address) -> BigInt {
    BalanceOf { owner: owner.to_vec() }
        .call(token.to_vec())
        .unwrap_or_else(|| panic!("balanceOf({owner}) eth_call failed for token {token}"))
}

fn seed_deltas(
    block: &eth::v2::Block,
    creation_tx: &B256,
    component_id: String,
    seeds: [(&Address, BigInt); 2],
) -> Result<[BalanceDelta; 2]> {
    let tx = block
        .transactions()
        .find(|tx| tx.hash == creation_tx.as_slice())
        .ok_or_else(|| anyhow!("creation tx {creation_tx} not found in block {}", block.number))?;
    Ok(seeds.map(|(token, seed)| BalanceDelta {
        // Seeds are the only deltas for their component's store keys in the creation
        // block (transfer tracking for these components starts strictly after it), so
        // the transaction ordinal cannot collide with a log ordinal on the same key.
        ord: tx.begin_ordinal,
        tx: Some(tx.into()),
        token: token.to_vec(),
        delta: seed.to_signed_bytes_be(),
        component_id: component_id.clone().into_bytes(),
    }))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};

    use super::*;

    fn test_config() -> Config {
        Config {
            psm: address!("f6e72db5454dd049d0788e411b06cfaf16853042"),
            psm_creation_block: 20283666,
            psm_creation_tx: b256!(
                "61e5d04f14d1fea9c505fb4dc9b6cf6e97bc83f2076b53cb7e92d0a2e88b6bbd"
            ),
            pocket: address!("37305b1cd40574e4c5ce33f8e8306be057fd7341"),
            wrapper: address!("a188eec8f81263234da3622a406892f3d630f98c"),
            wrapper_creation_block: 20668728,
            wrapper_creation_tx: b256!(
                "43ddae74123936f6737b78fcf785547f7f6b7b27e280fe7fbf98c81b3c018585"
            ),
            converter: address!("3225737a9bbb6473cb4a45b7244aca2befdb276a"),
            converter_creation_block: 20663734,
            converter_creation_tx: b256!(
                "b63d6f4cfb9945130ab32d914aaaafbad956be3718176771467b4154f9afab61"
            ),
            vat: address!("35d1b3f3d7966a1dfe207aa4514c12a259a0492b"),
            dai_join: address!("9759a6ac90977b93b58547b4a71c78317f391a28"),
            usds_join: address!("3c0f895007ca717aa01c8693e59df1e8c3777feb"),
            dai: address!("6b175474e89094c44da98b954eedeac495271d0f"),
            usdc: address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            usds: address!("dc035d45d973e3ec169d2276ddab16f1e407384f"),
        }
    }

    fn tx() -> Transaction {
        Transaction::default()
    }

    fn transfer(from: &[u8], to: &[u8], value: BigInt) -> Transfer {
        Transfer { from: from.to_vec(), to: to.to_vec(), value }
    }

    #[test]
    fn dai_transfer_to_psm_credits_psm_only() {
        let config = test_config();
        let user = [7u8; 20];
        let deltas = deltas_for_transfer(
            &config,
            21_000_000,
            true,
            config.dai.as_slice(),
            transfer(&user, config.psm.as_slice(), BigInt::from(1000)),
            5,
            &tx(),
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].component_id, config.psm_component_id().into_bytes());
        assert_eq!(deltas[0].delta, BigInt::from(1000).to_signed_bytes_be());
    }

    #[test]
    fn dai_mint_to_psm_credits_psm_inventory() {
        // The keeper `fill()` path: the escrow side of the same operation is tracked
        // separately from the DaiJoin `exit` call.
        let config = test_config();
        let deltas = deltas_for_transfer(
            &config,
            21_000_000,
            true,
            config.dai.as_slice(),
            transfer(Address::ZERO.as_slice(), config.psm.as_slice(), BigInt::from(500)),
            5,
            &tx(),
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].component_id, config.psm_component_id().into_bytes());
    }

    #[test]
    fn usdc_transfer_from_pocket_debits_psm() {
        let config = test_config();
        let user = [7u8; 20];
        let deltas = deltas_for_transfer(
            &config,
            21_000_000,
            true,
            config.usdc.as_slice(),
            transfer(config.pocket.as_slice(), &user, BigInt::from(42)),
            5,
            &tx(),
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].delta, BigInt::from(-42).to_signed_bytes_be());
    }

    #[test]
    fn self_transfer_and_zero_value_are_ignored() {
        let config = test_config();
        assert!(deltas_for_transfer(
            &config,
            21_000_000,
            true,
            config.dai.as_slice(),
            transfer(config.psm.as_slice(), config.psm.as_slice(), BigInt::from(1000)),
            5,
            &tx(),
        )
        .is_empty());
        let user = [7u8; 20];
        assert!(deltas_for_transfer(
            &config,
            21_000_000,
            true,
            config.dai.as_slice(),
            transfer(&user, config.psm.as_slice(), BigInt::zero()),
            5,
            &tx(),
        )
        .is_empty());
    }

    #[test]
    fn uncreated_components_emit_no_deltas() {
        let config = test_config();
        let user = [7u8; 20];
        // Transfer touching the PSM while the PSM component is not part of this run.
        assert!(deltas_for_transfer(
            &config,
            21_000_000,
            false,
            config.dai.as_slice(),
            transfer(&user, config.psm.as_slice(), BigInt::from(1000)),
            5,
            &tx(),
        )
        .iter()
        .all(|d| d.component_id != config.psm_component_id().into_bytes()));
    }

    #[test]
    fn psm_transfer_in_creation_block_is_inside_the_seed() {
        let config = test_config();
        let user = [7u8; 20];
        assert!(deltas_for_transfer(
            &config,
            config.psm_creation_block,
            true,
            config.dai.as_slice(),
            transfer(&user, config.psm.as_slice(), BigInt::from(1000)),
            5,
            &tx(),
        )
        .is_empty());
    }
}
