use crate::{ST_ETH_ADDRESS_IMPL, ST_ETH_ADDRESS_PROXY_COMPONENT_ID};
use substreams::{hex, prelude::*};
use substreams_ethereum::pb::eth;

const STORAGE_SLOT_BUFFERED_ETH: [u8; 32] =
    hex!("ed310af23f61f96daefbcd140b306c0bdbf8c178398299741687b90e794772b0");
const CL_BALANCE_POSITION: [u8; 32] =
    hex!("a66d35f054e68143c18f32c990ed5cb972bb68a68f500cd2dd3a16bbf3686483");
const CL_VALIDATORS_POSITION: [u8; 32] =
    hex!("9f70001d82b6ef54e9d3725b46581c3eb9ee3aa02b941b6aa54d678a9ca35b10");
const DEPOSITED_VALIDATORS_POSITION: [u8; 32] =
    hex!("e6e35175eb53fc006520a2a9c3e9711a7c00de6ff2c32dd31df8c5a24cac1b5c");

#[substreams::handlers::store]
fn store_storage_variables(params: String, block: eth::v2::Block, store: StoreSetBigInt) {
    let start_block: u64 = params
        .parse()
        .expect("Failed to parse start block parameter");

    // Initialize values at the start block. These values are taken from tx:
    // 0x1d93c1ffe57bcbd01c22bd31ce0d8bdb531a29f1a88461463232ebcf1a4458f6
    if block.number == start_block {
        store.set(
            0,
            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:buffered_eth"),
            &BigInt::from(
                num_bigint::BigInt::parse_bytes("6039330734020734919508".as_bytes(), 10)
                    .expect("Failed to parse initial buffered_eth value"),
            ),
        );
        store.set(
            0,
            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:cl_balance_position"),
            &BigInt::from(
                num_bigint::BigInt::parse_bytes("8627987356661781000000000".as_bytes(), 10)
                    .expect("Failed to parse initial cl_balance_position value"),
            ),
        );
        store.set(
            0,
            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:cl_validators_position"),
            &BigInt::from(
                num_bigint::BigInt::parse_bytes("408746".as_bytes(), 10)
                    .expect("Failed to parse initial cl_validators_position value"),
            ),
        );
        store.set(
            0,
            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:deposited_validators_position"),
            &BigInt::from(
                num_bigint::BigInt::parse_bytes("409420".as_bytes(), 10)
                    .expect("Failed to parse initial deposited_validators_position value"),
            ),
        );
    }

    for tx in block.transactions() {
        for call in tx.calls.iter() {
            if call.state_reverted {
                continue;
            }

            if call.address == ST_ETH_ADDRESS_IMPL {
                for storage_change in call.storage_changes.iter() {
                    let value = BigInt::from_unsigned_bytes_be(&storage_change.new_value);
                    if storage_change.key == STORAGE_SLOT_BUFFERED_ETH {
                        store.set(
                            call.begin_ordinal,
                            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:buffered_eth"),
                            &value,
                        );
                    } else if storage_change.key == CL_BALANCE_POSITION {
                        store.set(
                            call.begin_ordinal,
                            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:cl_balance_position"),
                            &value,
                        );
                    } else if storage_change.key == CL_VALIDATORS_POSITION {
                        store.set(
                            call.begin_ordinal,
                            format!("{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:cl_validators_position"),
                            &value,
                        );
                    } else if storage_change.key == DEPOSITED_VALIDATORS_POSITION {
                        store.set(
                            call.begin_ordinal,
                            format!(
                                "{ST_ETH_ADDRESS_PROXY_COMPONENT_ID}:deposited_validators_position"
                            ),
                            &value,
                        );
                    };
                }
            }
        }
    }
}
