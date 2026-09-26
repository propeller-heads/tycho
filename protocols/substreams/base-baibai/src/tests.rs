use super::*;
use alloy_primitives::address;
use substreams::pb::substreams::StoreDelta;
use substreams_ethereum::pb::eth::v2::{BlockHeader, Call, Log, StorageChange, TransactionTrace};

const BASE: Address = address!("4200000000000000000000000000000000000006");
const OTHER: Address = Address::repeat_byte(2);
const TAKER: Address = Address::repeat_byte(3);
const PARAMS: &str = "entrypoint=0x98c1d9e102eb2806d902b13186bdc7892ac4ffba&curve_book=0x604d9b9eb1e1571c78661a6c1088427ec9c8c6e5&custodian=0xaac48feb93c5c97e0fb3c7c57e1633922a4acda3&quote=0x833589fcd6edb6e08f4c7c32d4f71b54bda02913&start_block=50895895";

// Store reads at transaction boundaries, including pre-existing state at ordinal zero.
#[derive(Default)]
struct History(Vec<StoreDelta>);
impl History {
    fn set(&mut self, ordinal: u64, key: String, value: Vec<u8>) {
        let old = self.get(ordinal, &key);
        self.0.push(StoreDelta {
            ordinal,
            key,
            new_value: value,
            old_value: old.clone().unwrap_or_default(),
            operation: if old.is_some() { Operation::Update } else { Operation::Create } as i32,
        });
    }
    fn get(&self, ordinal: u64, key: &str) -> Option<Vec<u8>> {
        self.0
            .iter()
            .rev()
            .find(|d| d.ordinal <= ordinal && d.key == key)
            .map(|d| d.new_value.clone())
    }
    fn deltas(&self) -> StoreDeltas {
        StoreDeltas {
            deltas: self
                .0
                .iter()
                .filter(|d| d.ordinal > 0)
                .cloned()
                .collect(),
        }
    }
}
fn block(calls: Vec<Call>) -> Block {
    Block {
        number: VALIDATED_BLOCK + 1,
        header: Some(BlockHeader { timestamp: Some(Default::default()), ..Default::default() }),
        transaction_traces: vec![TransactionTrace {
            status: 1,
            begin_ordinal: 1,
            end_ordinal: 100,
            calls,
            ..Default::default()
        }],
        ..Default::default()
    }
}
fn event(address: Address, signature: &str, topics: &[Address], ordinal: u64) -> Log {
    Log {
        address: address.to_vec(),
        topics: std::iter::once(keccak256(signature).to_vec())
            .chain(
                topics
                    .iter()
                    .map(|a| a.into_word().to_vec()),
            )
            .collect(),
        ordinal,
        ..Default::default()
    }
}
fn listed(config: &Config, bases: &[Address]) -> (History, History, History) {
    let (mut state, mut keys, mut balances) =
        (History::default(), History::default(), History::default());
    keys.set(
        0,
        "pairs".into(),
        bases
            .iter()
            .map(|b| format!("{b:x};"))
            .collect::<String>()
            .into_bytes(),
    );
    for &base in bases {
        state.set(0, format!("pair:{base:x}"), vec![1]);
    }
    for token in bases
        .iter()
        .chain([config.quote].iter())
    {
        balances.set(0, balance_key(config, *token), b"100".to_vec());
    }
    (state, keys, balances)
}
fn output(
    config: &Config,
    block: &Block,
    state: &History,
    keys: &History,
    balances: &History,
) -> BlockChanges {
    protocol_changes(
        config,
        block,
        state.deltas(),
        balances.deltas(),
        &|o, k| state.get(o, k),
        &|o, k| keys.get(o, k),
        &|o, k| balances.get(o, k),
    )
    .unwrap()
}

#[test]
fn discovery_inherits_prior_fees_claims_and_words_and_repeats_do_not_recreate() {
    let config = Config::parse(PARAMS).unwrap();
    let (mut state, mut keys, mut balances) = listed(&config, &[BASE]);
    let slots = config.slots(OTHER);
    // Migration counters, TTL, and claims predate first SHAPE; seq need not be one.
    for (i, value) in [(0, 8), (1, 42), (30, 7), (31, 9)] {
        state.set(0, word_key(slots[i].0, slots[i].1), vec![value]);
    }
    for (base, value) in [(OTHER, vec![1, 0, 0]), (Address::ZERO, vec![1, 0, 25])] {
        keys.set(0, format!("fees:{base:x}"), format!("{TAKER:x};").into_bytes());
        state.set(0, format!("fee:{base:x}:{TAKER:x}"), value);
    }
    let tx = block(vec![Call {
        logs: vec![event(
            config.curve_book,
            "CurveUpdated(address,uint64,uint64,bytes32)",
            &[OTHER],
            20,
        )],
        ..Default::default()
    }]);
    for (o, k, v) in state_changes(&config, &tx) {
        state.set(o, k, v);
    }
    keys.set(20, "pairs".into(), format!("{BASE:x};{OTHER:x};").into_bytes());
    balances.set(1, balance_key(&config, OTHER), b"77".to_vec());
    let result = output(&config, &tx, &state, &keys, &balances);
    let change = &result.changes[0];
    assert_eq!(change.component_changes.len(), 1);
    assert_eq!(change.component_changes[0].id, config.id(OTHER));
    let attrs: HashMap<_, _> = change
        .entity_changes
        .iter()
        .find(|c| c.component_id == config.id(OTHER))
        .unwrap()
        .attributes
        .iter()
        .map(|a| (a.name.as_str(), a.value.clone()))
        .collect();
    assert_eq!(attrs["word_0"], vec![8]);
    assert_eq!(attrs["word_1"], vec![42]);
    assert_eq!(attrs["word_30"], vec![7]);
    assert_eq!(attrs["word_31"], vec![9]);
    assert_eq!(attrs[format!("pair_fee_{TAKER:x}").as_str()], vec![1, 0, 0]);
    assert_eq!(attrs[format!("taker_fee_{TAKER:x}").as_str()], vec![1, 0, 25]);
    assert_eq!(change.balance_changes.len(), 2);
    assert!(change
        .balance_changes
        .iter()
        .any(|b| b.token == OTHER.as_slice() && b.balance == vec![77]));
    state
        .0
        .iter_mut()
        .for_each(|d| d.ordinal = 0);
    state.set(30, format!("pair:{OTHER:x}"), vec![1]);
    assert!(output(&config, &tx, &state, &keys, &balances)
        .changes
        .iter()
        .all(|c| c.component_changes.is_empty()));
}

#[test]
fn shared_quote_balances_claims_ttl_and_taker_fees_fan_out() {
    let config = Config::parse(PARAMS).unwrap();
    let (mut state, keys, mut balances) = listed(&config, &[BASE, OTHER]);
    let slots = config.slots(BASE);
    for i in [0, 31] {
        state.set(20, word_key(slots[i].0, slots[i].1), vec![9]);
    }
    state.set(21, format!("fee:{:x}:{TAKER:x}", Address::ZERO), vec![1, 0, 50]);
    state.set(22, format!("fee:{BASE:x}:{TAKER:x}"), vec![1, 0, 0]);
    balances.set(23, balance_key(&config, config.quote), b"60".to_vec());
    let result = output(&config, &block(vec![]), &state, &keys, &balances);
    assert_eq!(result.changes[0].entity_changes.len(), 2);
    for entity in &result.changes[0].entity_changes {
        assert!(entity
            .attributes
            .iter()
            .any(|a| a.name == "word_31" && a.value == vec![9]));
        assert!(entity
            .attributes
            .iter()
            .any(|a| a.name == "word_0"));
        assert!(entity
            .attributes
            .iter()
            .any(|a| a.name == format!("taker_fee_{TAKER:x}")));
        assert_eq!(
            entity
                .attributes
                .iter()
                .any(|a| a.name == format!("pair_fee_{TAKER:x}")),
            entity.component_id == config.id(BASE)
        );
    }
    assert_eq!(result.changes[0].balance_changes.len(), 2);
    assert!(result.changes[0]
        .balance_changes
        .iter()
        .all(|b| b.token == config.quote.as_slice() && b.balance == vec![60]));
}

#[test]
fn events_and_storage_follow_execution_order_and_exclude_reverts() {
    let config = Config::parse(PARAMS).unwrap();
    let fee = |bps: Option<u16>, ordinal| {
        let mut log = event(
            config.entrypoint,
            if bps.is_some() {
                "TakerFeeSet(address,address,uint16)"
            } else {
                "TakerFeeCleared(address,address)"
            },
            &[TAKER, OTHER],
            ordinal,
        );
        log.data = bps.map_or(vec![], |b| {
            U256::from(b)
                .to_be_bytes::<32>()
                .to_vec()
        });
        log
    };
    let slot = config.slots(OTHER)[30].1;
    let write = |ordinal, value| StorageChange {
        address: config.custodian.to_vec(),
        key: slot.to_vec(),
        new_value: vec![value],
        ordinal,
        ..Default::default()
    };
    let tx = block(vec![
        Call {
            logs: vec![
                fee(Some(0), 20),
                fee(None, 30),
                event(
                    config.custodian,
                    "WithdrawSettled(address,address,uint256,uint256,uint64)",
                    &[TAKER, OTHER],
                    25,
                ),
            ],
            storage_changes: vec![write(24, 8)],
            ..Default::default()
        },
        Call { logs: vec![fee(Some(50), 10)], ..Default::default() },
        Call {
            logs: vec![
                fee(Some(90), 40),
                event(
                    config.curve_book,
                    "CurveUpdated(address,uint64,uint64,bytes32)",
                    &[OTHER],
                    41,
                ),
            ],
            storage_changes: vec![write(39, 99)],
            state_reverted: true,
            ..Default::default()
        },
    ]);
    let changes = state_changes(&config, &tx);
    assert_eq!(
        changes
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        vec![10, 20, 24, 30]
    );
    assert_eq!(changes[1].2, vec![1, 0, 0]);
    assert_eq!(changes[2].2, vec![8]);
    assert_eq!(changes[3].2, vec![0, 0, 0]);
    let mut failed = tx;
    failed.transaction_traces[0].status = 2;
    assert!(state_changes(&config, &failed).is_empty());
}

#[test]
fn bootstrap_reconciles_prelisting_deposits_and_same_block_transfers() {
    let config = Config::parse(PARAMS).unwrap();
    let (_, mut keys, _) = listed(&config, &[BASE]);
    keys.set(20, "pairs".into(), format!("{BASE:x};{OTHER:x};").into_bytes());
    let transfer = |from, to, amount, ordinal| {
        let mut log = event(OTHER, "Transfer(address,address,uint256)", &[from, to], ordinal);
        log.data = U256::from(amount)
            .to_be_bytes::<32>()
            .to_vec();
        log
    };
    let tx = block(vec![Call {
        logs: vec![
            transfer(TAKER, config.custodian, 7, 10),
            transfer(config.custodian, TAKER, 5, 30),
            transfer(config.custodian, config.custodian, 100, 40),
        ],
        ..Default::default()
    }]);
    let deltas = balance_deltas(&config, &tx, &|o, k| keys.get(o, k), &|token| {
        assert_eq!(token, OTHER);
        Ok(BigInt::from(52))
    })
    .unwrap()
    .balance_deltas;
    assert_eq!(BigInt::from_signed_bytes_be(&deltas[0].delta), BigInt::from(50));
    assert_eq!(
        deltas
            .iter()
            .fold(BigInt::zero(), |s, d| s + BigInt::from_signed_bytes_be(&d.delta)),
        BigInt::from(52)
    );
    assert!(deltas
        .windows(2)
        .all(|d| d[0].ord < d[1].ord));
    let store = BalanceStore::new();
    store_balance_changes(BlockBalanceDeltas { balance_deltas: deltas }, store.clone());
    assert_eq!(
        store
            .0
            .borrow()
            .get(&balance_key(&config, OTHER)),
        Some(&BigInt::from(52))
    );
}

#[test]
fn upgrades_pause_existing_and_later_discovered_pairs() {
    let config = Config::parse(PARAMS).unwrap();
    let (mut state, mut keys, mut balances) = listed(&config, &[BASE]);
    let tx = block(vec![Call {
        logs: vec![event(config.curve_book, "Upgraded(address)", &[OTHER], 10)],
        ..Default::default()
    }]);
    let mut validated = tx.clone();
    validated.number = VALIDATED_BLOCK;
    assert!(state_changes(&config, &validated).is_empty());
    for (o, k, v) in state_changes(&config, &tx) {
        state.set(o, k, v);
    }
    assert!(output(&config, &tx, &state, &keys, &balances).changes[0].entity_changes[0]
        .attributes
        .iter()
        .any(|a| a.name == "paused" && a.value == vec![1]));
    state
        .0
        .iter_mut()
        .for_each(|d| d.ordinal = 0);
    state.set(20, format!("pair:{OTHER:x}"), vec![1]);
    keys.set(20, "pairs".into(), format!("{BASE:x};{OTHER:x};").into_bytes());
    balances.set(1, balance_key(&config, OTHER), b"0".to_vec());
    let result = output(&config, &tx, &state, &keys, &balances);
    assert!(result.changes[0]
        .entity_changes
        .iter()
        .filter(|e| e.component_id == config.id(OTHER))
        .flat_map(|e| &e.attributes)
        .any(|a| a.name == "paused" && a.value == vec![1]));
}

#[test]
fn tracks_weth_wraps_and_unwraps_without_rpc_for_known_tokens() {
    let config = Config::parse(PARAMS).unwrap();
    let (_, keys, _) = listed(&config, &[BASE]);
    let log = |signature, amount, ordinal| {
        let mut log = event(BASE, signature, &[config.custodian], ordinal);
        log.data = U256::from(amount)
            .to_be_bytes::<32>()
            .to_vec();
        log
    };
    let tx = block(vec![
        Call {
            logs: vec![
                log("Deposit(address,uint256)", 9, 10),
                log("Withdrawal(address,uint256)", 4, 20),
            ],
            ..Default::default()
        },
        Call {
            logs: vec![log("Deposit(address,uint256)", 99, 30)],
            state_reverted: true,
            ..Default::default()
        },
    ]);
    let deltas = balance_deltas(&config, &tx, &|o, k| keys.get(o, k), &|_| {
        panic!("known token must not bootstrap")
    })
    .unwrap()
    .balance_deltas;
    assert_eq!(deltas.len(), 2);
    assert_eq!(BigInt::from_signed_bytes_be(&deltas[0].delta), BigInt::from(9));
    assert_eq!(BigInt::from_signed_bytes_be(&deltas[1].delta), BigInt::from(-4));
}

#[test]
fn creation_and_updates_use_each_transaction_boundary() {
    let config = Config::parse(PARAMS).unwrap();
    let (mut state, mut keys, mut balances) = listed(&config, &[BASE]);
    let mut block = block(vec![]);
    block
        .transaction_traces
        .push(TransactionTrace {
            status: 1,
            index: 1,
            begin_ordinal: 101,
            end_ordinal: 200,
            ..Default::default()
        });
    let slot = config.slots(OTHER)[3];
    state.set(10, word_key(slot.0, slot.1), vec![1]);
    state.set(20, format!("pair:{OTHER:x}"), vec![1]);
    keys.set(20, "pairs".into(), format!("{BASE:x};{OTHER:x};").into_bytes());
    balances.set(1, balance_key(&config, OTHER), b"77".to_vec());
    state.set(120, word_key(slot.0, slot.1), vec![2]);
    balances.set(130, balance_key(&config, OTHER), b"88".to_vec());
    let result = output(&config, &block, &state, &keys, &balances);
    assert_eq!(result.changes.len(), 2);
    for (i, tx) in result.changes.iter().enumerate() {
        assert_eq!(tx.component_changes.len(), usize::from(i == 0));
        assert!(tx
            .entity_changes
            .iter()
            .filter(|e| e.component_id == config.id(OTHER))
            .flat_map(|e| &e.attributes)
            .any(|a| a.name == "word_3" && a.value == vec![1 + i as u8]));
        assert!(tx
            .balance_changes
            .iter()
            .any(|b| b.token == OTHER.as_slice() && b.balance == vec![77 + 11 * i as u8]));
    }
}

// The SDK mock is cfg(test)-private to that crate; capture its public additive-store writes.
#[derive(Clone, Default)]
struct BalanceStore(std::rc::Rc<std::cell::RefCell<HashMap<String, BigInt>>>);
impl StoreNew for BalanceStore {
    fn new() -> Self {
        Self::default()
    }
}
impl StoreDelete for BalanceStore {
    fn delete_prefix(&self, _ordinal: i64, prefix: &String) {
        self.0
            .borrow_mut()
            .retain(|key, _| !key.starts_with(prefix));
    }
}
impl StoreAdd<BigInt> for BalanceStore {
    fn add<K: AsRef<str>>(&self, _ordinal: u64, key: K, value: BigInt) {
        let mut values = self.0.borrow_mut();
        let balance = values
            .entry(key.as_ref().into())
            .or_insert_with(BigInt::zero);
        *balance = balance.clone() + value;
    }
    fn add_many<K: AsRef<str>>(&self, ordinal: u64, keys: &Vec<K>, value: BigInt) {
        for key in keys {
            self.add(ordinal, key, value.clone());
        }
    }
}
