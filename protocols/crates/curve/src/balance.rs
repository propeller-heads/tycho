//! Absolute component balances, ported from the substreams `map_relative_balances` /
//! `store_balances` pair.
//!
//! The substreams accumulate signed transfer deltas into a store that has been running since
//! genesis, then emit the store's absolute value. Here the running total is seeded from the
//! confirmed stream instead, and a pending block's transfer logs are applied on top. Seed plus
//! deltas equals the store's value, so the emitted bytes match the substreams'.

use std::collections::HashMap;

use num_bigint::BigInt;
use tycho_common::{
    models::blockchain::{LogInput, TxInput},
    Bytes,
};

use crate::registry::{normalize_eth, PoolRegistry};

/// sUSD moved to a new contract; the substreams credit transfers of the old one to the new token,
/// which is what the pool's component token list holds.
const OLD_SUSD: [u8; 20] = hex_address("57ab1e02fee23774580c119740129eac7081e9d3");
const NEW_SUSD: [u8; 20] = hex_address("57ab1ec28d129707052df4df418d58a2d46d5f51");

/// Running absolute balances per `(normalized pool id, token)`.
///
/// Values stay signed: the Curve substreams emit `to_signed_bytes_be()` of the store value
/// without clamping negatives, unlike `tycho_substreams::balances::aggregate_balances_changes`.
/// Clamping here would produce different bytes for a pool whose store has gone negative.
#[derive(Debug, Default, Clone)]
pub struct BalanceTracker {
    balances: HashMap<(String, Bytes), BigInt>,
}

impl BalanceTracker {
    /// Records the confirmed absolute balance of `token` in the pool keyed by `pool_key`.
    pub fn set(&mut self, pool_key: String, token: Bytes, balance: &Bytes) {
        self.balances.insert(
            (pool_key, normalize_eth(token)),
            BigInt::from_signed_bytes_be(balance.as_ref()),
        );
    }

    pub fn forget_pool(&mut self, pool_key: &str) {
        self.balances
            .retain(|(id, _), _| id != pool_key);
    }

    /// Absolute balances after applying every transfer in `txs`, for the tokens they touch.
    ///
    /// Only `(pool, token)` pairs a transaction actually moved are returned, mirroring the
    /// substreams, which emit a balance change only where its store saw a delta.
    pub fn pending(
        &self,
        registry: &PoolRegistry,
        txs: &[TxInput],
    ) -> Vec<PendingComponentBalance> {
        let mut running: HashMap<(String, Bytes), BigInt> = HashMap::new();
        let mut last_tx: HashMap<(String, Bytes), Bytes> = HashMap::new();

        let mut ordered: Vec<&TxInput> = txs
            .iter()
            .filter(|tx| tx.succeeded())
            .collect();
        ordered.sort_by_key(|tx| tx.index());

        for tx in ordered {
            for delta in transfer_deltas(tx, registry) {
                let key = (delta.pool_key, delta.token);
                let balance = running
                    .entry(key.clone())
                    .or_insert_with(|| {
                        self.balances
                            .get(&key)
                            .cloned()
                            .unwrap_or_else(BigInt::default)
                    });
                *balance += delta.delta;
                last_tx.insert(key, tx.hash().clone());
            }
        }

        running
            .into_iter()
            .map(|((pool_key, token), balance)| PendingComponentBalance {
                modify_tx: last_tx
                    .get(&(pool_key.clone(), token.clone()))
                    .cloned()
                    .unwrap_or_default(),
                pool_key,
                token,
                balance,
            })
            .collect()
    }
}

/// One pool's absolute balance of one token after a pending block.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingComponentBalance {
    pub pool_key: String,
    pub token: Bytes,
    pub balance: BigInt,
    /// Hash of the last transaction that moved this balance.
    pub modify_tx: Bytes,
}

/// A single signed balance movement decoded from one log.
struct TransferDelta {
    pool_key: String,
    token: Bytes,
    delta: BigInt,
}

/// Signed balance movements in `tx` that affect a tracked pool's own token balances.
///
/// Mirrors `tycho_substreams::balances::extract_balance_deltas_from_tx` with the Curve predicate:
/// the transactor must be a tracked pool and the transferred token one of that pool's tokens.
fn transfer_deltas(tx: &TxInput, registry: &PoolRegistry) -> Vec<TransferDelta> {
    let mut deltas = Vec::new();
    for log in tx.logs() {
        let token = normalize_eth(substitute_susd(log.address()));
        match decode_transfer_log(log) {
            Some(Movement::Transfer { from, to, value }) => {
                push_delta(&mut deltas, registry, &from, &token, -value.clone());
                push_delta(&mut deltas, registry, &to, &token, value);
            }
            Some(Movement::Deposit { dst, value }) => {
                push_delta(&mut deltas, registry, &dst, &token, value);
            }
            Some(Movement::Withdrawal { src, value }) => {
                push_delta(&mut deltas, registry, &src, &token, -value);
            }
            None => {}
        }
    }
    deltas
}

fn push_delta(
    deltas: &mut Vec<TransferDelta>,
    registry: &PoolRegistry,
    transactor: &Bytes,
    token: &Bytes,
    delta: BigInt,
) {
    let pool_key = crate::registry::normalize_id(&hex::encode(transactor.as_ref()));
    let Some(pool) = registry.get(&pool_key) else { return };
    if !pool.tokens.contains(token) {
        return;
    }
    deltas.push(TransferDelta { pool_key, token: token.clone(), delta });
}

/// A token movement carried by a single log.
enum Movement {
    Transfer { from: Bytes, to: Bytes, value: BigInt },
    Deposit { dst: Bytes, value: BigInt },
    Withdrawal { src: Bytes, value: BigInt },
}

/// Decodes the three log shapes the substreams balance helper understands.
///
/// The topic counts and data length are checked exactly as the generated substreams decoders do,
/// so a non-standard event that they skip is skipped here too.
fn decode_transfer_log(log: &LogInput) -> Option<Movement> {
    let topic0 = log.topics().first()?.as_ref();
    let value = (log.data().len() == 32).then(|| unsigned(log.data().as_ref()))?;
    match (topic0, log.topics().len()) {
        (TRANSFER_TOPIC, 3) => Some(Movement::Transfer {
            from: topic_address(log, 1)?,
            to: topic_address(log, 2)?,
            value,
        }),
        (DEPOSIT_TOPIC, 2) => Some(Movement::Deposit { dst: topic_address(log, 1)?, value }),
        (WITHDRAWAL_TOPIC, 2) => Some(Movement::Withdrawal { src: topic_address(log, 1)?, value }),
        _ => None,
    }
}

/// `keccak256("Transfer(address,address,uint256)")`
const TRANSFER_TOPIC: &[u8] =
    &hex_bytes32("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
/// `keccak256("Deposit(address,uint256)")`
const DEPOSIT_TOPIC: &[u8] =
    &hex_bytes32("e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c");
/// `keccak256("Withdrawal(address,uint256)")`
const WITHDRAWAL_TOPIC: &[u8] =
    &hex_bytes32("7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65");

/// The low 20 bytes of an indexed address topic.
fn topic_address(log: &LogInput, index: usize) -> Option<Bytes> {
    let topic = log.topics().get(index)?.as_ref();
    (topic.len() == 32).then(|| Bytes::from(topic[12..].to_vec()))
}

fn unsigned(bytes: &[u8]) -> BigInt {
    BigInt::from_bytes_be(num_bigint::Sign::Plus, bytes)
}

/// Credits transfers of the retired sUSD contract to the token the pool actually lists.
fn substitute_susd(token: &Bytes) -> Bytes {
    if token.as_ref() == OLD_SUSD {
        Bytes::from(NEW_SUSD.to_vec())
    } else {
        token.clone()
    }
}

const fn hex_address(text: &str) -> [u8; 20] {
    let bytes = text.as_bytes();
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = hex_nibble(bytes[i * 2]) << 4 | hex_nibble(bytes[i * 2 + 1]);
        i += 1;
    }
    out
}

const fn hex_bytes32(text: &str) -> [u8; 32] {
    let bytes = text.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = hex_nibble(bytes[i * 2]) << 4 | hex_nibble(bytes[i * 2 + 1]);
        i += 1;
    }
    out
}

const fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("hex literal must be lower-case hex"),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::keccak256;
    use tycho_common::models::protocol::ProtocolComponent;
    use tycho_simulation::evm::{engine_db::tycho_db::PreCachedDB, simulation::SimulationEngine};

    use super::*;

    const POOL: [u8; 20] = hex_address("bebc44782c7db0a1a60cb6fe97d0b483032ff1c7");
    const DAI: [u8; 20] = hex_address("6b175474e89094c44da98b954eedeac495271d0f");
    const USDC: [u8; 20] = hex_address("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");

    fn pool_id() -> String {
        hex::encode(POOL)
    }

    /// A registry holding 3pool with DAI/USDC, registered without touching the VM: 3pool's
    /// variant comes from the legacy address table, so probing never runs.
    fn registry() -> PoolRegistry {
        let mut registry = PoolRegistry::default();
        let component = ProtocolComponent {
            id: pool_id(),
            tokens: vec![Bytes::from(DAI.to_vec()), Bytes::from(USDC.to_vec())],
            static_attributes: HashMap::from([(
                "coins".to_string(),
                Bytes::from(
                    format!(r#"["0x{}","0x{}"]"#, hex::encode(DAI), hex::encode(USDC)).into_bytes(),
                ),
            )]),
            ..Default::default()
        };
        let engine: SimulationEngine<PreCachedDB> =
            SimulationEngine::new(PreCachedDB::new().unwrap(), false);
        assert!(registry.register(&pool_id(), &component, &engine), "3pool must register");
        registry
    }

    fn topic(text: &str) -> Bytes {
        Bytes::from(keccak256(text.as_bytes()).to_vec())
    }

    fn address_topic(address: [u8; 20]) -> Bytes {
        let mut padded = vec![0u8; 12];
        padded.extend_from_slice(&address);
        Bytes::from(padded)
    }

    fn amount(value: u64) -> Bytes {
        Bytes::from(
            alloy::primitives::U256::from(value)
                .to_be_bytes::<32>()
                .to_vec(),
        )
    }

    fn transfer(token: [u8; 20], from: [u8; 20], to: [u8; 20], value: u64) -> LogInput {
        LogInput::new(
            Bytes::from(token.to_vec()),
            vec![
                topic("Transfer(address,address,uint256)"),
                address_topic(from),
                address_topic(to),
            ],
            amount(value),
            0,
        )
    }

    fn tx(index: u64, logs: Vec<LogInput>) -> TxInput {
        TxInput::new(
            Bytes::from(vec![index as u8; 32]),
            Bytes::from(vec![0u8; 20]),
            Bytes::from(POOL.to_vec()),
            index,
            logs,
            true,
        )
    }

    #[test]
    fn test_incoming_transfer_adds_to_the_confirmed_balance() {
        let registry = registry();
        let mut tracker = BalanceTracker::default();
        tracker.set(pool_id(), Bytes::from(DAI.to_vec()), &Bytes::from(vec![100u8]));

        let pending = tracker.pending(&registry, &[tx(0, vec![transfer(DAI, USDC, POOL, 5)])]);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].balance, BigInt::from(105));
        assert_eq!(pending[0].token, Bytes::from(DAI.to_vec()));
    }

    #[test]
    fn test_outgoing_transfer_subtracts_and_may_go_negative() {
        let registry = registry();
        let tracker = BalanceTracker::default();

        let pending = tracker.pending(&registry, &[tx(0, vec![transfer(DAI, POOL, USDC, 7)])]);

        // Unclamped: the Curve substreams emit the signed store value, so a tracker seeded
        // without a confirmed balance must report the negative rather than zero.
        assert_eq!(pending[0].balance, BigInt::from(-7));
    }

    #[test]
    fn test_deltas_accumulate_across_transactions_in_index_order() {
        let registry = registry();
        let mut tracker = BalanceTracker::default();
        tracker.set(pool_id(), Bytes::from(DAI.to_vec()), &Bytes::from(vec![50u8]));

        let pending = tracker.pending(
            &registry,
            &[
                tx(1, vec![transfer(DAI, POOL, USDC, 20)]),
                tx(0, vec![transfer(DAI, USDC, POOL, 10)]),
            ],
        );

        assert_eq!(pending.len(), 1, "one (pool, token) pair");
        assert_eq!(pending[0].balance, BigInt::from(40), "50 + 10 - 20");
        assert_eq!(
            pending[0].modify_tx,
            Bytes::from(vec![1u8; 32]),
            "the highest-index transaction is the last writer"
        );
    }

    #[test]
    fn test_untracked_transactors_and_tokens_are_ignored() {
        let registry = registry();
        let tracker = BalanceTracker::default();
        let stranger = [0x99u8; 20];

        let pending = tracker.pending(
            &registry,
            &[tx(
                0,
                vec![
                    // Right token, but neither side is a tracked pool.
                    transfer(DAI, stranger, stranger, 1),
                    // Tracked pool, but a token it does not hold.
                    transfer(stranger, stranger, POOL, 1),
                ],
            )],
        );

        assert!(pending.is_empty(), "got {pending:?}");
    }

    #[test]
    fn test_reverted_transactions_are_skipped() {
        let registry = registry();
        let tracker = BalanceTracker::default();
        let reverted = TxInput::new(
            Bytes::from(vec![0u8; 32]),
            Bytes::from(vec![0u8; 20]),
            Bytes::from(POOL.to_vec()),
            0,
            vec![transfer(DAI, USDC, POOL, 5)],
            false,
        );

        assert!(tracker
            .pending(&registry, &[reverted])
            .is_empty());
    }

    #[test]
    fn test_old_susd_transfers_credit_the_new_token() {
        let mut registry = PoolRegistry::default();
        let susd_pool = hex_address("a5407eae9ba41422680e2e00537571bcc53efbfd");
        let component = ProtocolComponent {
            id: hex::encode(susd_pool),
            tokens: vec![Bytes::from(NEW_SUSD.to_vec()), Bytes::from(DAI.to_vec())],
            static_attributes: HashMap::from([(
                "coins".to_string(),
                Bytes::from(
                    format!(r#"["0x{}","0x{}"]"#, hex::encode(NEW_SUSD), hex::encode(DAI))
                        .into_bytes(),
                ),
            )]),
            ..Default::default()
        };
        let engine: SimulationEngine<PreCachedDB> =
            SimulationEngine::new(PreCachedDB::new().unwrap(), false);
        assert!(registry.register(&hex::encode(susd_pool), &component, &engine));

        let pending = BalanceTracker::default()
            .pending(&registry, &[tx(0, vec![transfer(OLD_SUSD, DAI, susd_pool, 9)])]);

        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].token,
            Bytes::from(NEW_SUSD.to_vec()),
            "the old contract's transfer must land on the new token"
        );
        assert_eq!(pending[0].balance, BigInt::from(9));
    }

    #[test]
    fn test_weth_deposit_and_withdrawal_move_the_balance() {
        let weth = hex_address("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let mut registry = PoolRegistry::default();
        let component = ProtocolComponent {
            id: pool_id(),
            tokens: vec![Bytes::from(weth.to_vec()), Bytes::from(DAI.to_vec())],
            static_attributes: HashMap::from([(
                "coins".to_string(),
                Bytes::from(
                    format!(r#"["0x{}","0x{}"]"#, hex::encode(weth), hex::encode(DAI)).into_bytes(),
                ),
            )]),
            ..Default::default()
        };
        let engine: SimulationEngine<PreCachedDB> =
            SimulationEngine::new(PreCachedDB::new().unwrap(), false);
        assert!(registry.register(&pool_id(), &component, &engine));

        let deposit = LogInput::new(
            Bytes::from(weth.to_vec()),
            vec![topic("Deposit(address,uint256)"), address_topic(POOL)],
            amount(30),
            0,
        );
        let withdrawal = LogInput::new(
            Bytes::from(weth.to_vec()),
            vec![topic("Withdrawal(address,uint256)"), address_topic(POOL)],
            amount(12),
            1,
        );

        let pending =
            BalanceTracker::default().pending(&registry, &[tx(0, vec![deposit, withdrawal])]);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].balance, BigInt::from(18), "30 deposited, 12 withdrawn");
    }

    /// The hard-coded topics must match the real event signatures.
    #[test]
    fn test_topic_constants_match_their_signatures() {
        assert_eq!(TRANSFER_TOPIC, topic("Transfer(address,address,uint256)").as_ref());
        assert_eq!(DEPOSIT_TOPIC, topic("Deposit(address,uint256)").as_ref());
        assert_eq!(WITHDRAWAL_TOPIC, topic("Withdrawal(address,uint256)").as_ref());
    }

    #[test]
    fn test_malformed_logs_decode_to_nothing() {
        let short_data = LogInput::new(
            Bytes::from(DAI.to_vec()),
            vec![
                topic("Transfer(address,address,uint256)"),
                address_topic(USDC),
                address_topic(POOL),
            ],
            Bytes::from(vec![1u8; 16]),
            0,
        );
        let wrong_topic_count = LogInput::new(
            Bytes::from(DAI.to_vec()),
            vec![topic("Transfer(address,address,uint256)"), address_topic(POOL)],
            amount(1),
            0,
        );

        assert!(decode_transfer_log(&short_data).is_none(), "value must be a full word");
        assert!(decode_transfer_log(&wrong_topic_count).is_none(), "from and to must be indexed");
    }

    /// Guards the const hex parser the address and topic constants are built with.
    #[test]
    fn test_hex_address_parses_lowercase_hex() {
        assert_eq!(
            hex_address("6b175474e89094c44da98b954eedeac495271d0f").to_vec(),
            hex::decode("6b175474e89094c44da98b954eedeac495271d0f").unwrap()
        );
    }
}
