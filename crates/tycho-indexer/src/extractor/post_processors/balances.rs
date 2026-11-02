use num_bigint::BigUint;
use num_traits::Num;
use tycho_common::Bytes;

use crate::extractor::{models::BlockChanges, u256_num::bytes_to_f64};

fn transcode_ascii_balance_to_be(ascii_encoded: &Bytes) -> anyhow::Result<Bytes> {
    let ascii_string = String::from_utf8(ascii_encoded.clone().to_vec())
        .map_err(|e| anyhow::format_err!("Invalid UTF-8 sequence: {ascii_encoded}: {e}"))?;

    if ascii_string.starts_with('-') {
        Ok(Bytes::zero(32))
    } else {
        let as_integer = BigUint::from_str_radix(&ascii_string, 10)
            .map_err(|e| anyhow::format_err!("Invalid integer: {e}"))?;

        let mut result = [0u8; 32];
        let integer_bytes = as_integer.to_bytes_be();

        // Copy the bytes into the result array, starting from the right (big-endian)
        let start = 32 - integer_bytes.len();
        result[start..].copy_from_slice(&integer_bytes);

        Ok(Bytes::from(result))
    }
}

fn transcode_le_balance_to_be(le_encoded: &Bytes) -> anyhow::Result<Bytes> {
    let mut be_encoded = le_encoded.clone().to_vec();
    be_encoded.reverse(); // Reverse the vector to convert from little-endian to big-endian
    Ok(Bytes::from(be_encoded))
}

pub fn transcode_ambient_balances(mut changes: BlockChanges) -> BlockChanges {
    changes
        .txs_with_update
        .iter_mut()
        .for_each(|tx_changes| {
            tx_changes
                .balance_changes
                .iter_mut()
                .for_each(|(_, balance)| {
                    balance
                        .iter_mut()
                        .for_each(|(_, value)| {
                            value.balance = transcode_ascii_balance_to_be(&value.balance)
                                .expect("Balance transcoding failed");
                            value.balance_float = bytes_to_f64(value.balance.as_ref())
                                .expect("failed converting balance to float");
                        });
                });
        });
    changes
}

/// This post processor allow us to ignore any component balance change if the component id and the
/// token are the same.
///
/// We had to add this for Balancer because when a EulerLinearPool is created it returns the minted
/// pool tokens in the balance changes.
/// TODO: look into this and see if we can fix it on the substreams side.
pub fn ignore_self_balances(mut changes: BlockChanges) -> BlockChanges {
    changes
        .txs_with_update
        .iter_mut()
        .for_each(|tx_changes| {
            tx_changes
                .balance_changes
                .iter_mut()
                .for_each(|(_, balance)| {
                    balance
                        .retain(|_, value| format!("{:#020x}", value.token) != value.component_id);
                });
        });
    changes
}

#[deprecated]
pub fn transcode_usv2_balances(mut changes: BlockChanges) -> BlockChanges {
    changes
        .txs_with_update
        .iter_mut()
        .for_each(|tx_changes| {
            tx_changes
                .balance_changes
                .iter_mut()
                .for_each(|(_, balance)| {
                    balance
                        .iter_mut()
                        .for_each(|(_, value)| {
                            value.balance = transcode_le_balance_to_be(&value.balance).unwrap();
                            value.balance_float = bytes_to_f64(value.balance.as_ref())
                                .expect("failed converting balance to float");
                        });
                });
        });
    changes
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use tycho_common::models::{
        blockchain::{Transaction, TxWithChanges},
        contract::AccountBalance,
        protocol::ComponentBalance,
        Chain,
    };

    use super::*;
    use crate::testing::block;

    #[test]
    fn test_trancode_ascii_balance_to_be() {
        let ascii_balance = Bytes::from("0x373131383430313238");

        let be_balance = transcode_ascii_balance_to_be(&ascii_balance).unwrap();

        assert_eq!(be_balance, Bytes::from(711840128u128).lpad(32, 0));
    }

    #[test]
    fn test_transcode_le_balance_to_be() {
        let le_balance = Bytes::from("0xb55cb8eef8db0b5c");

        let be_balance = transcode_le_balance_to_be(&le_balance).unwrap();

        assert_eq!(be_balance, Bytes::from("0x5c0bdbf8eeb85cb5"));
    }

    #[test]
    fn test_ignore_self_balances() {
        let txs_with_update = vec![TxWithChanges {
            balance_changes: HashMap::from([(
                "0xabc".to_string(),
                HashMap::from([
                    (
                        Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716").unwrap(),
                        ComponentBalance {
                            token: Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            balance_float: 36522027799.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            component_id: "0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399".to_string(),
                        },
                    ),
                    (
                        Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        ComponentBalance {
                            token: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            balance_float: 36522027799.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            component_id: "0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399".to_string(),
                        },
                    ),
                ]),
            )]),
            account_balance_changes: HashMap::from([(
                Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                HashMap::from([
                    (
                        Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716").unwrap(),
                        AccountBalance {
                            token: Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            account: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        },
                    ),
                    (
                        Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        AccountBalance {
                            token: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            account: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        },
                    ),
                ]),
            )]),
            tx: Transaction::new(
                Bytes::zero(32),
                Bytes::zero(32),
                Bytes::zero(20),
                Some(Bytes::zero(20)),
                10,
            ),
            ..Default::default()
        }];

        let changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            block(1),
            0,
            false,
            txs_with_update,
            Vec::new(),
        );

        // expected to skip itself as a component balance, but still track it as an account balance
        let expected = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            block(1),
            0,
            false,
            vec![TxWithChanges {
                balance_changes: HashMap::from([(
                    "0xabc".to_string(),
                    HashMap::from([(
                        Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716").unwrap(),
                        ComponentBalance {
                            token: Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            balance_float: 36522027799.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            component_id: "0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399".to_string(),
                        },
                    )]),
                )]),
            account_balance_changes: HashMap::from([(
                Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                HashMap::from([
                    (
                        Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716").unwrap(),
                        AccountBalance {
                            token: Bytes::from_str("0xeb91861f8a4e1c12333f42dce8fb0ecdc28da716")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            account: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        },
                    ),
                    (
                        Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        AccountBalance {
                            token: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399")
                                .unwrap(),
                            balance: Bytes::from(0_i32.to_be_bytes()),
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                            account: Bytes::from_str("0xd4e7c1f3da1144c9e2cfd1b015eda7652b4a4399").unwrap(),
                        },
                    ),
                ]),
            )]),
                tx: Transaction::new(
                    Bytes::zero(32),
                    Bytes::zero(32),
                    Bytes::zero(20),
                    Some(Bytes::zero(20)),
                    10,
                ),
                ..Default::default()
            }],
            Vec::new(),
        );

        let processed = ignore_self_balances(changes);

        assert_eq!(processed, expected)
    }
}
