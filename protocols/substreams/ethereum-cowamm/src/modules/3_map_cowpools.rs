use crate::pb::cowamm::{BindingChangeType, CowPool, CowPoolBind, CowPoolCreations, CowPools};
use anyhow::{bail, Context, Ok, Result};
use prost::Message;
use substreams::store::{StoreGet, StoreGetString};

/// Replays a pool's `;`-delimited binding-change history (hex-encoded `CowPoolBind`
/// protobufs, ordinal-ordered) and returns the bindings that are still active.
pub fn parse_binds(history: &str) -> Result<Vec<CowPoolBind>> {
    let mut active_binds: Vec<CowPoolBind> = Vec::new();
    for chunk in history.split(';') {
        // The append store writes a trailing delimiter after every entry.
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let raw = hex::decode(chunk).context("invalid hex in CowAMM binding history")?;
        let bind = CowPoolBind::decode(raw.as_slice())
            .context("failed to decode CowAMM binding change")?;
        if bind.tx.is_none() {
            bail!(
                "CowAMM binding change for pool 0x{} is missing its transaction",
                hex::encode(&bind.address)
            );
        }
        active_binds.retain(|active| active.token != bind.token);
        match BindingChangeType::from_i32(bind.change_type) {
            Some(BindingChangeType::Bind) => active_binds.push(bind),
            Some(BindingChangeType::Unbind) => {}
            Some(BindingChangeType::Unspecified) | None => {
                bail!("invalid CowAMM binding change type {}", bind.change_type)
            }
        }
    }
    Ok(active_binds)
}

#[substreams::handlers::map]
pub fn map_cowpools(
    creations: CowPoolCreations,
    binds: StoreGetString,
) -> Result<CowPools, substreams::errors::Error> {
    let mut pools: Vec<CowPool> = Vec::new();

    let creations = &creations;
    let binds = &binds;

    for creation in creations.pools.iter() {
        let base_key = hex::encode(&creation.address);
        let bind_history = match binds.get_at(creation.ordinal, &base_key) {
            Some(data) => data,
            None => continue, // skip if no bind found
        };

        let parsed_binds = parse_binds(&bind_history)?;
        if parsed_binds.len() != 2 {
            continue;
        }
        let bind1 = &parsed_binds[0];
        let bind2 = &parsed_binds[1];

        let (token_a, weight_a, token_b, weight_b) = if bind1.token < bind2.token {
            (&bind1.token, &bind1.weight, &bind2.token, &bind2.weight)
        } else {
            (&bind2.token, &bind2.weight, &bind1.token, &bind1.weight)
        };
        pools.push(CowPool {
            address: creation.address.clone(),
            token_a: token_a.clone(),
            token_b: token_b.clone(),
            lp_token: creation.lp_token.clone(),
            weight_a: weight_a.to_vec(),
            weight_b: weight_b.to_vec(),
            fee: 0,
            created_tx_hash: creation.created_tx_hash.clone(),
        });
    }

    Ok(CowPools { pools })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{modules::store_cowpool_binds::encode_binding_change, pb::cowamm::Transaction};
    use hex_literal::hex;

    fn binding_change(
        token: Vec<u8>,
        weight: Vec<u8>,
        amount: Vec<u8>,
        ordinal: u64,
        change_type: BindingChangeType,
    ) -> CowPoolBind {
        CowPoolBind {
            address: hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1").to_vec(),
            token,
            weight,
            amount,
            tx: Some(Transaction {
                from: hex!("1234567890123456789012345678901234567890").to_vec(),
                to: hex!("abcdefabcdefabcdefabcdefabcdefabcdefabcd").to_vec(),
                hash: hex!("fedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedc")
                    .to_vec(),
                index: ordinal,
            }),
            ordinal,
            change_type: change_type.into(),
        }
    }

    fn history(changes: &[CowPoolBind]) -> String {
        changes
            .iter()
            .map(encode_binding_change)
            .collect::<Vec<_>>()
            .join(";")
    }

    #[test]
    fn test_parse_binds_roundtrips_a_single_bind() {
        let change = binding_change(
            hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").to_vec(),
            hex!("0de0b6b3a7640000").to_vec(),
            hex!("01").to_vec(),
            2,
            BindingChangeType::Bind,
        );

        let binds = parse_binds(&history(std::slice::from_ref(&change)))
            .expect("binding history should parse");

        assert_eq!(binds, vec![change]);
    }

    #[test]
    fn test_parse_binds_keeps_multiple_active_bindings() {
        let token_a = hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").to_vec();
        let token_b = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0").to_vec();
        let changes = [
            binding_change(
                token_a.clone(),
                hex!("01").to_vec(),
                hex!("0a").to_vec(),
                1,
                BindingChangeType::Bind,
            ),
            binding_change(
                token_b.clone(),
                hex!("02").to_vec(),
                hex!("14").to_vec(),
                2,
                BindingChangeType::Bind,
            ),
        ];

        let binds = parse_binds(&history(&changes)).expect("binding history should parse");

        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].token, token_a);
        assert_eq!(binds[1].token, token_b);
    }

    #[test]
    fn test_parse_binds_returns_only_active_bindings_after_rebind() {
        let token_a = hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").to_vec();
        let token_b = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0").to_vec();
        let changes = [
            binding_change(
                token_a.clone(),
                hex!("01").to_vec(),
                hex!("0a").to_vec(),
                1,
                BindingChangeType::Bind,
            ),
            binding_change(token_a.clone(), vec![], vec![], 2, BindingChangeType::Unbind),
            binding_change(
                token_b,
                hex!("02").to_vec(),
                hex!("14").to_vec(),
                3,
                BindingChangeType::Bind,
            ),
            binding_change(
                token_a.clone(),
                hex!("03").to_vec(),
                hex!("1e").to_vec(),
                4,
                BindingChangeType::Bind,
            ),
        ];

        let binds = parse_binds(&history(&changes)).expect("two bindings should remain active");

        assert_eq!(binds.len(), 2);
        let rebound = binds
            .iter()
            .find(|bind| bind.token == token_a)
            .expect("rebound token should be active");
        assert_eq!(rebound.weight, hex!("03"));
        assert_eq!(rebound.amount, hex!("1e"));
    }

    #[test]
    fn test_parse_binds_rejects_malformed_history() {
        let error = parse_binds("not-hex").expect_err("non-hex history should error");
        assert!(error
            .to_string()
            .contains("invalid hex in CowAMM binding history"));

        let error = parse_binds("deadbeef").expect_err("non-protobuf bytes should error");
        assert!(error
            .to_string()
            .contains("failed to decode CowAMM binding change"));
    }

    #[test]
    fn test_parse_binds_rejects_binding_change_without_transaction() {
        let mut change = binding_change(
            hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").to_vec(),
            hex!("01").to_vec(),
            hex!("0a").to_vec(),
            1,
            BindingChangeType::Bind,
        );
        change.tx = None;

        let error = parse_binds(&history(&[change])).expect_err("missing tx should error");
        assert!(error
            .to_string()
            .contains("missing its transaction"));
    }
}
