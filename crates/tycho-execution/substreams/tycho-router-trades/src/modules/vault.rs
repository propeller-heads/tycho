//! ERC-6909 vault balance movements on the routers.
//!
//! A router credits a fee recipient inside its own vault rather than transferring the token
//! out, so router revenue accrues there. The credit itself carries no ERC-6909 `Transfer`
//! (`Vault._creditVaultForFees` mints without an event to save gas, and `_takeFees` emits
//! `FeesTaken` instead), which is why this module covers only the movements that do emit one:
//! deposits, withdrawals, and moves between two owners.
use anyhow::Result;
use substreams::scalar::BigInt;
use substreams_ethereum::{pb::eth::v2::Block, Event};

use super::block_timestamp;
use crate::{
    abi::tycho_router_v3_1::events::Transfer,
    params::{Params, RouterVersion},
    pb::tycho::router::v1::{VaultTransfer, VaultTransfers},
};

/// Emits every ERC-6909 `Transfer` a configured router logged in a committed transaction.
#[substreams::handlers::map]
pub fn map_vault_transfers(params: String, block: Block) -> Result<VaultTransfers> {
    let params = Params::parse(&params)?;
    Ok(VaultTransfers { transfers: vault_transfers(&params, &block) })
}

/// The vault movements of one block, reading only the receipts of successful transactions so a
/// log the chain threw away never becomes a balance.
fn vault_transfers(params: &Params, block: &Block) -> Vec<VaultTransfer> {
    let timestamp = block_timestamp(block);
    let mut transfers = Vec::new();
    for tx in block.transactions() {
        for log in tx
            .receipt
            .as_ref()
            .map(|r| r.logs.as_slice())
            .unwrap_or_default()
        {
            // V2 has no vault, so a matching topic from one is a collision, not a movement.
            let Some(router) = params
                .router(&log.address)
                .filter(|r| r.version != RouterVersion::V2)
            else {
                continue;
            };
            let Some(ev) = Transfer::match_and_decode(log) else {
                continue;
            };
            let Some(token) = token_of(&ev.id) else {
                substreams::log::info!(
                    "vault id {} is not a token address, tx 0x{} log {}",
                    ev.id,
                    hex::encode(&tx.hash),
                    log.index
                );
                continue;
            };
            transfers.push(VaultTransfer {
                chain: params.chain.clone(),
                block_number: block.number,
                block_timestamp: timestamp,
                tx_hash: tx.hash.clone(),
                log_index: log.index,
                router: router.address.clone(),
                caller: ev.caller,
                sender: ev.sender.clone(),
                receiver: ev.receiver.clone(),
                token,
                amount: ev.amount.to_string(),
                kind: kind_of(&ev.sender, &ev.receiver).to_string(),
            });
        }
    }
    transfers
}

/// The token an ERC-6909 id stands for, or `None` when the id is not one.
///
/// `Vault._toId` casts the token address to `uint256`, so every id a router mints fits in 20
/// bytes and converts straight back.
fn token_of(id: &BigInt) -> Option<Vec<u8>> {
    let bytes = id.to_bytes_be().1;
    if bytes.len() > 20 {
        return None;
    }
    let mut token = vec![0u8; 20 - bytes.len()];
    token.extend_from_slice(&bytes);
    Some(token)
}

/// Which side of the balance the movement is on, read from the zero address a mint or a burn
/// puts in place of an owner.
fn kind_of(sender: &[u8], receiver: &[u8]) -> &'static str {
    match (sender.iter().all(|b| *b == 0), receiver.iter().all(|b| *b == 0)) {
        (true, _) => "credit",
        (false, true) => "debit",
        (false, false) => "transfer",
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{Log, TransactionReceipt, TransactionTrace};

    use super::*;

    const V3_ROUTER: &str = "1111111111111111111111111111111111111111";
    const V2_ROUTER: &str = "2222222222222222222222222222222222222222";
    // keccak256("Transfer(address,address,address,uint256,uint256)")
    const TRANSFER: &str = "1b3d7edb2e9c0b0e7c525b20aaaef0f5940d2ed71663c7d39266ecafac728859";
    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn params() -> Params {
        Params::parse(&format!("chain=ethereum&routers=0x{V3_ROUTER}:v3_1,0x{V2_ROUTER}:v2"))
            .unwrap()
    }

    fn word(hex_tail: &str) -> Vec<u8> {
        hex::decode(format!("{:0>64}", hex_tail)).unwrap()
    }

    /// A `Transfer` log: sender, receiver and id are indexed, caller and amount are in the data.
    fn transfer_log(emitter: &str, sender: &str, receiver: &str, id: &str, amount: u64) -> Log {
        let mut data = word("9999999999999999999999999999999999999999");
        data.extend_from_slice(&word(&format!("{amount:x}")));
        Log {
            address: hex::decode(emitter).unwrap(),
            topics: vec![word(TRANSFER), word(sender), word(receiver), word(id)],
            data,
            index: 7,
            ..Default::default()
        }
    }

    fn block(status: i32, logs: Vec<Log>) -> Block {
        Block {
            number: 42,
            transaction_traces: vec![TransactionTrace {
                status,
                receipt: Some(TransactionReceipt { logs, ..Default::default() }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn succeeded(logs: Vec<Log>) -> Block {
        block(1, logs)
    }

    #[test]
    fn a_mint_on_a_v3_router_is_a_credit() {
        let owner = "3333333333333333333333333333333333333333";
        let log = transfer_log(V3_ROUTER, "0", owner, USDC, 250);
        let transfers = vault_transfers(&params(), &succeeded(vec![log]));
        assert_eq!(transfers.len(), 1);
        let t = &transfers[0];
        assert_eq!(t.kind, "credit");
        assert_eq!(t.amount, "250");
        assert_eq!(hex::encode(&t.token), USDC);
        assert_eq!(hex::encode(&t.receiver), owner);
        assert_eq!(hex::encode(&t.router), V3_ROUTER);
        assert_eq!(t.block_number, 42);
        assert_eq!(t.log_index, 7);
    }

    #[test]
    fn a_burn_is_a_debit_and_a_move_is_a_transfer() {
        let a = "3333333333333333333333333333333333333333";
        let b = "4444444444444444444444444444444444444444";
        let kinds: Vec<String> = vault_transfers(
            &params(),
            &succeeded(vec![
                transfer_log(V3_ROUTER, a, "0", USDC, 1),
                transfer_log(V3_ROUTER, a, b, USDC, 2),
            ]),
        )
        .into_iter()
        .map(|t| t.kind)
        .collect();
        assert_eq!(kinds, ["debit", "transfer"]);
    }

    #[test]
    fn a_v2_router_has_no_vault() {
        let log = transfer_log(V2_ROUTER, "0", "3333333333333333333333333333333333333333", USDC, 1);
        assert!(vault_transfers(&params(), &succeeded(vec![log])).is_empty());
    }

    #[test]
    fn an_unconfigured_emitter_is_ignored() {
        let other = "5555555555555555555555555555555555555555";
        let log = transfer_log(other, "0", "3333333333333333333333333333333333333333", USDC, 1);
        assert!(vault_transfers(&params(), &succeeded(vec![log])).is_empty());
    }

    #[test]
    fn a_failed_transaction_moves_nothing() {
        let log = transfer_log(V3_ROUTER, "0", "3333333333333333333333333333333333333333", USDC, 1);
        assert!(vault_transfers(&params(), &block(2, vec![log])).is_empty());
    }

    #[test]
    fn an_id_wider_than_an_address_is_skipped() {
        let wide = "ff".repeat(21);
        let log =
            transfer_log(V3_ROUTER, "0", "3333333333333333333333333333333333333333", &wide, 1);
        assert!(vault_transfers(&params(), &succeeded(vec![log])).is_empty());
    }

    /// Two logs taken off ethereum mainnet, so the topic order and the data layout are checked
    /// against what the routers really emit rather than against this module's own idea of it.
    ///
    /// The withdrawal is the zero-address balance the fee taker took out of the first router in
    /// block 25144849. It moved 487837220983021421 wei of ETH to that address, which is how the
    /// zero id on that router is known to be native ETH.
    #[test]
    fn decodes_logs_taken_off_mainnet() {
        let v3_0 = "1f8db310f32d48b6180ff902ec60c586128cef47";
        let v3_1 = "ea290ce3eae57bdb37e57872a5a14dc0d2f6e614";
        let fee_taker = "a5503d92ee16a78e602996ad362674dc49034a14";
        let depositor = "85957b990db8b266bc03fb0cfe295a060ebbf6e1";
        let params =
            Params::parse(&format!("chain=ethereum&routers=0x{v3_0}:v3_0,0x{v3_1}:v3_1")).unwrap();

        let withdrawal = Log {
            address: hex::decode(v3_0).unwrap(),
            topics: vec![word(TRANSFER), word(fee_taker), word("0"), word("0")],
            data: hex::decode(format!("{:0>64}{:0>64}", fee_taker, "6c5255e28e81f6d")).unwrap(),
            index: 332,
            ..Default::default()
        };
        let deposit = Log {
            address: hex::decode(v3_1).unwrap(),
            topics: vec![word(TRANSFER), word("0"), word(depositor), word(USDC)],
            data: hex::decode(format!("{:0>64}{:0>64}", depositor, "f4240")).unwrap(),
            index: 963,
            ..Default::default()
        };

        let transfers = vault_transfers(&params, &succeeded(vec![withdrawal, deposit]));
        assert_eq!(transfers.len(), 2);

        assert_eq!(transfers[0].kind, "debit");
        assert_eq!(transfers[0].amount, "487837220983021421");
        assert_eq!(hex::encode(&transfers[0].sender), fee_taker);
        assert_eq!(hex::encode(&transfers[0].caller), fee_taker);
        assert_eq!(transfers[0].token, vec![0u8; 20]);

        assert_eq!(transfers[1].kind, "credit");
        assert_eq!(transfers[1].amount, "1000000");
        assert_eq!(hex::encode(&transfers[1].receiver), depositor);
        assert_eq!(hex::encode(&transfers[1].token), USDC);
    }

    #[test]
    fn zero_id_is_the_zero_address() {
        assert_eq!(token_of(&BigInt::zero()), Some(vec![0u8; 20]));
    }

    #[test]
    fn id_converts_back_to_the_token_address() {
        let token = hex::decode(USDC).unwrap();
        assert_eq!(token_of(&BigInt::from_unsigned_bytes_be(&token)), Some(token));
    }
}
