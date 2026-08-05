use crate::{
    abi::{
        liquidity_admin::events::LogUpdateExchangePrices, liquidity_user_module::events::LogOperate,
    },
    events,
    modules::utils::Params,
    pb::tycho::evm::fluid_v2::Pool,
};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use itertools::Itertools;
use num_bigint::{BigUint, Sign};
use num_traits::{ToPrimitive, Zero};
use std::{collections::HashMap, vec};
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetArray, StoreGetProto},
};
use substreams_ethereum::{
    pb::eth::v2::{self as eth},
    Event,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    protocol_components: BlockChanges,
    pools_store: StoreGetProto<Pool>,
    balance_store: StoreDeltas,
    balance_deltas: BlockBalanceDeltas,
    token_to_pools_store: StoreGetArray<String>,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let block_ts = block
        .header
        .as_ref()
        .and_then(|h| h.timestamp.as_ref())
        .map(|t| t.seconds as u64)
        .expect("Invalid block timestamp");
    let dex_v2_address = hex::decode(&params.dex_v2_address).expect("Invalid dex_v2_address");
    let liquidity_address =
        hex::decode(&params.liquidity_address).expect("Invalid liquidity_address");

    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    for change in protocol_components.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        change
            .component_changes
            .iter()
            .for_each(|c| {
                let component_change = c.clone();
                let pool_key = format!("{}:{}", "Pool", component_change.id);

                if let Some(pool) = pools_store.get_last(pool_key) {
                    if pool.created_tx_hash == tx.hash {
                        builder.add_protocol_component(&component_change);
                    }
                }
            });
        change
            .entity_changes
            .iter()
            .for_each(|c| {
                builder.add_entity_change(c);
            });
    }
    aggregate_balances_changes(balance_store, balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    for trx in block.transactions() {
        let tx_index: u64 = trx.index.into();
        for (log, call_view) in trx.logs_with_calls() {
            if log.address == dex_v2_address {
                if let Some((component_id, attrs)) = events::get_log_changed_attributes(
                    log,
                    &call_view.call.storage_changes,
                    &dex_v2_address,
                ) {
                    let builder = transaction_changes
                        .entry(tx_index)
                        .or_insert_with(|| {
                            let tx = Transaction {
                                to: trx.to.clone(),
                                from: trx.from.clone(),
                                hash: trx.hash.clone(),
                                index: tx_index,
                            };
                            TransactionChangesBuilder::new(&tx)
                        });
                    builder.add_entity_change(&EntityChanges { component_id, attributes: attrs });
                }
            }
            if log.address == liquidity_address {
                // The liquidity contract stores exchange prices per token
                // (borrow_exchange_price/supply_exchange_price). These prices can
                // be actively updated by admins (LogUpdateExchangePrices event), or
                // passively affected by other operations through MoneyMarket Contract (LogOperate
                // event).
                if let Some(update_exchange_prices) = LogUpdateExchangePrices::match_and_decode(log)
                {
                    let changes = exchange_price_changes(
                        &update_exchange_prices.token,
                        &update_exchange_prices.supply_exchange_price,
                        &update_exchange_prices.borrow_exchange_price,
                        &pools_store,
                        &token_to_pools_store,
                    );
                    let builder = transaction_changes
                        .entry(tx_index)
                        .or_insert_with(|| {
                            let tx = Transaction {
                                to: trx.to.clone(),
                                from: trx.from.clone(),
                                hash: trx.hash.clone(),
                                index: tx_index,
                            };
                            TransactionChangesBuilder::new(&tx)
                        });
                    for change in changes {
                        builder.add_entity_change(&change);
                    }
                }

                if let Some(operate) = LogOperate::match_and_decode(log) {
                    let (supply_exchange_price, borrow_exchange_price) =
                        match calc_exchange_prices(&operate.exchange_prices_and_config, block_ts) {
                            Ok(prices) => prices,
                            Err(e) => {
                                substreams::log::info!(
                                    "Skipping LogOperate exchange price update: {}",
                                    e
                                );
                                continue;
                            }
                        };
                    let changes = exchange_price_changes(
                        &operate.token,
                        &supply_exchange_price,
                        &borrow_exchange_price,
                        &pools_store,
                        &token_to_pools_store,
                    );
                    let builder = transaction_changes
                        .entry(tx_index)
                        .or_insert_with(|| {
                            let tx = Transaction {
                                to: trx.to.clone(),
                                from: trx.from.clone(),
                                hash: trx.hash.clone(),
                                index: tx_index,
                            };
                            TransactionChangesBuilder::new(&tx)
                        });
                    for change in changes {
                        builder.add_entity_change(&change);
                    }
                }
            }
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: vec![],
    })
}

fn exchange_price_changes(
    token: &[u8],
    supply_price: &BigInt,
    borrow_price: &BigInt,
    pools_store: &StoreGetProto<Pool>,
    token_to_pools_store: &StoreGetArray<String>,
) -> Vec<EntityChanges> {
    let token_key = hex::encode(token);
    let component_ids = match token_to_pools_store.get_last(token_key) {
        Some(component_ids) => component_ids,
        None => return vec![],
    };

    let mut changes = Vec::new();
    for component_id_b64 in component_ids {
        let component_id_bytes = match STANDARD_NO_PAD.decode(component_id_b64) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let component_id = component_id_bytes.to_hex();
        let pool_key = format!("Pool:{component_id}");
        let is_token_0 = pools_store
            .get_last(pool_key)
            .map(|pool| pool.token0 == *token)
            .unwrap_or(false);

        let (borrow_name, supply_name) = if is_token_0 {
            ("token0/borrow_exchange_price", "token0/supply_exchange_price")
        } else {
            ("token1/borrow_exchange_price", "token1/supply_exchange_price")
        };

        changes.push(EntityChanges {
            component_id: component_id.clone(),
            attributes: vec![Attribute {
                name: borrow_name.to_string(),
                value: borrow_price.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            }],
        });
        changes.push(EntityChanges {
            component_id,
            attributes: vec![Attribute {
                name: supply_name.to_string(),
                value: supply_price.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            }],
        });
    }

    changes
}

// This implementation is based on the Solidity library LiquidityCalcs and mirrors its
// calcExchangePrices logic.
fn calc_exchange_prices(
    exchange_prices_and_config: &BigInt,
    now_ts: u64,
) -> Result<(BigInt, BigInt), anyhow::Error> {
    const SECONDS_PER_YEAR: u64 = 365 * 24 * 60 * 60;
    const FOUR_DECIMALS: u64 = 10_000;

    const X14: u64 = 0x3fff;
    const X15: u64 = 0x7fff;
    const X16: u64 = 0xffff;
    const X33: u64 = 0x1ffffffff;
    const X64: u64 = 0xffffffffffffffff;

    const BITS_EXCHANGE_PRICES_LAST_TIMESTAMP: u32 = 58;
    const BITS_EXCHANGE_PRICES_SUPPLY_EXCHANGE_PRICE: u32 = 91;
    const BITS_EXCHANGE_PRICES_BORROW_EXCHANGE_PRICE: u32 = 155;
    const BITS_EXCHANGE_PRICES_SUPPLY_RATIO: u32 = 219;
    const BITS_EXCHANGE_PRICES_BORROW_RATIO: u32 = 234;
    const BITS_EXCHANGE_PRICES_UTILIZATION: u32 = 30;
    const BITS_EXCHANGE_PRICES_FEE: u32 = 16;

    let (sign, bytes) = exchange_prices_and_config.to_bytes_be();
    if sign == Sign::Minus {
        anyhow::bail!("exchange_prices_and_config must be unsigned");
    }

    let value = BigUint::from_bytes_be(&bytes);

    let x64 = BigUint::from(X64);
    let x16 = BigUint::from(X16);
    let x15 = BigUint::from(X15);
    let x14 = BigUint::from(X14);
    let x33 = BigUint::from(X33);

    let mut supply_exchange_price = (&value >> BITS_EXCHANGE_PRICES_SUPPLY_EXCHANGE_PRICE) & &x64;
    let mut borrow_exchange_price = (&value >> BITS_EXCHANGE_PRICES_BORROW_EXCHANGE_PRICE) & &x64;

    if supply_exchange_price.is_zero() || borrow_exchange_price.is_zero() {
        anyhow::bail!("exchange price is zero");
    }

    let borrow_rate = &value & &x16;
    let last_ts = ((&value >> BITS_EXCHANGE_PRICES_LAST_TIMESTAMP) & &x33)
        .to_u64()
        .unwrap_or(0);
    let seconds_since_last_update = now_ts.saturating_sub(last_ts);

    let mut borrow_ratio = (&value >> BITS_EXCHANGE_PRICES_BORROW_RATIO) & &x15;

    if seconds_since_last_update == 0 || borrow_rate.is_zero() || borrow_ratio == BigUint::from(1u8)
    {
        return Ok((
            BigInt::from_unsigned_bytes_be(&supply_exchange_price.to_bytes_be()),
            BigInt::from_unsigned_bytes_be(&borrow_exchange_price.to_bytes_be()),
        ));
    }

    let seconds_since = BigUint::from(seconds_since_last_update);
    let seconds_per_year = BigUint::from(SECONDS_PER_YEAR);
    let four_decimals = BigUint::from(FOUR_DECIMALS);

    borrow_exchange_price += (&borrow_exchange_price * &borrow_rate * &seconds_since) /
        (&seconds_per_year * &four_decimals);

    let mut temp = (&value >> BITS_EXCHANGE_PRICES_SUPPLY_RATIO) & &x15;
    if temp == BigUint::from(1u8) {
        return Ok((
            BigInt::from_unsigned_bytes_be(&supply_exchange_price.to_bytes_be()),
            BigInt::from_unsigned_bytes_be(&borrow_exchange_price.to_bytes_be()),
        ));
    }

    let utilization = (&value >> BITS_EXCHANGE_PRICES_UTILIZATION) & &x14;
    let one_e27 = BigUint::from(10u64).pow(27u32);
    let one_e54 = BigUint::from(10u64).pow(54u32);

    if (&temp & BigUint::from(1u8)) == BigUint::from(1u8) {
        temp >>= 1u32;
        temp = (&one_e27 * &four_decimals) / &temp;
        temp = (&utilization * (&one_e27 + temp)) / &four_decimals;
    } else {
        temp >>= 1u32;
        temp =
            (&one_e27 * &utilization * (&four_decimals + temp)) / (&four_decimals * &four_decimals);
    }

    if (&borrow_ratio & BigUint::from(1u8)) == BigUint::from(1u8) {
        borrow_ratio >>= 1u32;
        borrow_ratio = (&borrow_ratio * &one_e27) / (&four_decimals + &borrow_ratio);
    } else {
        borrow_ratio >>= 1u32;
        borrow_ratio = &one_e27 - ((&borrow_ratio * &one_e27) / (&four_decimals + &borrow_ratio));
    }

    temp = (&four_decimals * temp * &borrow_ratio) / &one_e54;

    let fee = (&value >> BITS_EXCHANGE_PRICES_FEE) & &x14;
    temp = &borrow_rate * temp * (&four_decimals - fee);

    let denom = &seconds_per_year * &four_decimals * &four_decimals * &four_decimals;
    supply_exchange_price += (&supply_exchange_price * temp * &seconds_since) / denom;

    Ok((
        BigInt::from_unsigned_bytes_be(&supply_exchange_price.to_bytes_be()),
        BigInt::from_unsigned_bytes_be(&borrow_exchange_price.to_bytes_be()),
    ))
}

#[cfg(test)]
mod tests {
    use super::calc_exchange_prices;
    use num_bigint::BigUint;
    use std::str::FromStr;
    use substreams::scalar::BigInt;

    fn pack_exchange_prices(supply: u64, borrow: u64) -> BigInt {
        const SUPPLY_SHIFT: u32 = 91;
        const BORROW_SHIFT: u32 = 155;

        let value =
            (BigUint::from(supply) << SUPPLY_SHIFT) | (BigUint::from(borrow) << BORROW_SHIFT);
        BigInt::from_unsigned_bytes_be(&value.to_bytes_be())
    }

    #[test]
    fn extract_exchange_prices_basic() {
        let packed =
            BigInt::from_str("46464094416348171314390299771166327281840865656352815973125")
                .unwrap();
        let (supply_out, borrow_out) = calc_exchange_prices(&packed, 1764889531).unwrap();
        assert_eq!(supply_out.to_u64(), 1_010_498_400_696u64);
        assert_eq!(borrow_out.to_u64(), 1_017_362_263_006u64);
    }

    #[test]
    fn extract_exchange_prices_max_values() {
        let supply = u64::MAX;
        let borrow = u64::MAX - 1;
        let packed = pack_exchange_prices(supply, borrow);

        let (supply_out, borrow_out) = calc_exchange_prices(&packed, 1764889531).unwrap();
        assert_eq!(supply_out.to_u64(), supply);
        assert_eq!(borrow_out.to_u64(), borrow);
    }
}
