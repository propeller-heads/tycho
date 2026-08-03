use std::collections::{HashMap, HashSet, VecDeque};

use chrono::NaiveDateTime;
use deepsize::DeepSizeOf;
use tracing::{debug, error, instrument, trace, warn, Level};
use tycho_common::{
    models::{
        blockchain::{Block, BlockScoped},
        contract::AccountBalance,
        protocol::ComponentBalance,
        Address, AttrStoreKey, ComponentId, StoreVal,
    },
    storage::{BlockIdentifier, BlockOrTimestamp, StorageError},
    Bytes,
};

#[derive(Clone, Debug, Copy)]
pub enum BlockNumberOrTimestamp {
    Number(u64),
    Timestamp(NaiveDateTime),
}

impl BlockNumberOrTimestamp {
    fn greater_than(&self, other: &Block) -> bool {
        match self {
            BlockNumberOrTimestamp::Number(n) => n > &other.number,
            BlockNumberOrTimestamp::Timestamp(ts) => ts > &other.ts,
        }
    }
}

impl TryFrom<BlockOrTimestamp> for BlockNumberOrTimestamp {
    type Error = StorageError;

    fn try_from(value: BlockOrTimestamp) -> Result<Self, Self::Error> {
        Ok(match value {
            BlockOrTimestamp::Block(bid) => match bid {
                BlockIdentifier::Number((_, no)) => BlockNumberOrTimestamp::Number(no as u64),
                BlockIdentifier::Hash(_) => {
                    return Err(StorageError::Unexpected("BlockHash unsupported!".to_string()))
                }
                BlockIdentifier::Latest(_) => {
                    return Err(StorageError::Unexpected("Latest marker unsupported!".to_string()))
                }
            },
            BlockOrTimestamp::Timestamp(ts) => BlockNumberOrTimestamp::Timestamp(ts),
        })
    }
}

/// Buffer that holds blocks awaiting persistence to the database so extractors can batch commits
/// and efficiently handle chain reorganisations (reorg) without requiring database rollbacks.
///
/// Each new block received by an extractor is pushed here. Once enough finalized blocks accumulate
/// they are drained to the database.
///
/// In case of a chain reorg, we can just purge this buffer up to the common ancestor block.
#[derive(DeepSizeOf)]
pub(crate) struct ReorgBuffer<B>
where
    B: BlockScoped + DeepSizeOf,
{
    block_messages: VecDeque<B>,
    strict: bool,
}

/// The commitment status of a block or block-scoped data with the DB.
#[derive(PartialEq, Clone, Debug, Copy)]
pub enum CommitStatus {
    /// Versions at this status should have been committed to the db.
    Committed,
    /// Versions with this status should be in the reorg buffer.
    Uncommitted,
    /// We have not seen this version yet.
    Unseen,
}

impl<B> ReorgBuffer<B>
where
    B: BlockScoped + std::fmt::Debug + DeepSizeOf,
{
    pub(crate) fn new() -> Self {
        Self { block_messages: VecDeque::new(), strict: false }
    }

    /// Inserts a new block into the buffer. Ensures the new block is the expected next block,
    /// otherwise panics.
    pub fn insert_block(&mut self, new: B) -> Result<(), StorageError> {
        // Make sure the new block matches the one we expect, panic if not.
        if let Some(last_message) = self.block_messages.back() {
            if last_message.block().hash != new.block().parent_hash {
                return Err(StorageError::Unexpected(format!(
                    "Unexpected block sequence. Expected parent hash {} received {}",
                    last_message.block().hash,
                    new.block().parent_hash
                )));
            };
        }

        self.block_messages.push_back(new);

        Ok(())
    }

    /// Drains blocks up to the specified block height (non-inclusive) from the buffer. Returns the
    /// drained blocks ordered by ascending number or an error if the target height is not found
    /// in the buffer and strict mode is enabled.
    pub fn drain_blocks_until(
        &mut self,
        drain_upto_block_height: u64,
    ) -> Result<Vec<B>, StorageError> {
        let target_index = self.find_index(|b| b.block().number == drain_upto_block_height);
        let first = self
            .get_block_range(None, None)?
            .next()
            .map(|e| e.block().number);

        if let Some(idx) = target_index {
            // Drain and return every block before the target index.
            let mut temp = self.block_messages.split_off(idx);
            std::mem::swap(&mut self.block_messages, &mut temp);
            trace!(?temp, "ReorgBuffer drained blocks");
            Ok(temp.into())
        } else if !self.strict && first.unwrap_or(0) < drain_upto_block_height {
            warn!(
                ?first,
                ?drain_upto_block_height,
                "First uncommitted block not found in ReorgBuffer"
            );
            Ok(Vec::new())
        } else {
            error!(
                ?first,
                ?drain_upto_block_height,
                "First uncommitted block not found in ReorgBuffer. Strict mode is {}",
                self.strict
            );
            Err(StorageError::NotFound("block".into(), drain_upto_block_height.to_string()))
        }
    }

    fn find_index<F: Fn(&B) -> bool>(&self, predicate: F) -> Option<usize> {
        for (index, block_message) in self.block_messages.iter().enumerate() {
            if predicate(block_message) {
                return Some(index);
            }
        }
        None
    }

    /// Purges all blocks following the specified block hash from the buffer. Returns the purged
    /// blocks ordered by ascending number or an error if the target hash is not found.
    pub fn purge(&mut self, target_hash: Bytes) -> Result<Vec<B>, StorageError> {
        debug!("Purging reorg buffer... Target hash {}", target_hash.to_string());
        let mut target_index = None;

        for (index, block_message) in self
            .block_messages
            .iter()
            .rev()
            .enumerate()
        {
            if block_message.block().hash == target_hash {
                target_index = Some(self.block_messages.len() - index);
                break;
            }
        }

        if let Some(idx) = target_index {
            let purged = self
                .block_messages
                .split_off(idx)
                .into();
            trace!(?purged, "ReorgBuffer purged blocks");
            Ok(purged)
        } else {
            Err(StorageError::NotFound("block".into(), target_hash.to_string()))
        }
    }

    /// Returns an `Option` containing the most recent block in the buffer or `None` if the buffer
    /// is empty
    pub fn get_most_recent_block(&self) -> Option<tycho_common::models::blockchain::Block> {
        if let Some(block_message) = self.block_messages.back() {
            return Some(block_message.block());
        }
        None
    }

    /// Returns the number of blocks in the buffer with a block number less than the specified
    /// target block number. Assumes blocks are stored in ascending order.
    pub fn count_blocks_before(&self, target_block: u64) -> usize {
        self.block_messages
            .iter()
            .take_while(|msg| msg.block().number < target_block)
            .count()
    }

    /// Retrieves a range of blocks from the buffer.
    ///
    /// The retrieved iterator will include both the start and end block of specified range. In case
    /// either start or end block are None, the iterator will start the range at the oldest / end
    /// the range at the latest block.
    /// If the end version is a timestamp, it will be compared to the timestamp of the last block in
    /// the buffer. If the timestamp is greater than the last block's timestamp, the range will be
    /// returned from the start of the buffer to the last block.
    #[instrument(skip(self), level = Level::DEBUG, fields(buffered_range))]
    pub fn get_block_range(
        &self,
        start_version: Option<BlockNumberOrTimestamp>,
        end_version: Option<BlockNumberOrTimestamp>,
    ) -> Result<impl Iterator<Item = &B>, StorageError> {
        let buffered_range = match (self.block_messages.front(), self.block_messages.back()) {
            (Some(front), Some(back)) => {
                format!("{}-{}", front.block().number, back.block().number)
            }
            _ => "None".to_string(),
        };
        tracing::Span::current().record("buffered_range", buffered_range.as_str());

        let start_index = if let Some(version) = start_version {
            self.find_index(|b| !version.greater_than(&b.block()))
                .ok_or_else(|| {
                    StorageError::NotFound("Block".to_string(), format!("{version:?}"))
                })?
        } else {
            0
        };

        let end_index = match end_version {
            Some(version) => {
                // Handle case where timestamp is beyond last block.
                // Additionally, latest ts (NOW) is always beyond last block.
                if let Some(last) = self.block_messages.back() {
                    if let BlockNumberOrTimestamp::Timestamp(ts) = version {
                        if ts > last.block().ts {
                            return Ok(self.block_messages.range(start_index..));
                        }
                    }
                }

                let end_idx = self
                    .find_index(|b| !version.greater_than(&b.block()))
                    .ok_or_else(|| {
                        StorageError::NotFound("Block".to_string(), format!("{version:?}"))
                    })?;

                if end_idx < start_index {
                    return Err(StorageError::Unexpected(
                        "ReorgBuffer: Invalid block range".to_string(),
                    ));
                }
                end_idx + 1
            }
            None => self.block_messages.len(),
        };

        Ok(self
            .block_messages
            .range(start_index..end_index))
    }

    /// Retrieves [CommitStatus] for a block, returns None if the buffer is empty.
    pub fn get_commit_status(&self, version: BlockNumberOrTimestamp) -> Option<CommitStatus> {
        let first_block = self.block_messages.front();
        let last_block = self.block_messages.back();
        match (first_block, last_block) {
            (Some(first), Some(last)) => {
                let first_block = first.block();
                let last_block = last.block();

                if !version.greater_than(&first_block) {
                    Some(CommitStatus::Committed)
                } else if (version.greater_than(&first_block)) &
                    (!version.greater_than(&last_block))
                {
                    Some(CommitStatus::Uncommitted)
                } else {
                    Some(CommitStatus::Unseen)
                }
            }
            _ => None,
        }
    }
}

pub type ProtocolStateIdType = ComponentId;
pub type ProtocolStateKeyType = AttrStoreKey;
pub type ProtocolStateValueType = StoreVal;
pub type AccountStateIdType = Bytes;
pub type AccountStateKeyType = Bytes;
pub type AccountStateValueType = Bytes;

/// A ReorgBuffer entry containing state updates.
///
/// Enables additional state lookup methods within the buffer.
pub(crate) trait StateUpdateBufferEntry: std::fmt::Debug {
    fn get_filtered_protocol_state_update(
        &self,
        keys: Vec<(&ProtocolStateIdType, &ProtocolStateKeyType)>,
    ) -> HashMap<(ProtocolStateIdType, ProtocolStateKeyType), ProtocolStateValueType>;

    #[allow(clippy::mutable_key_type)]
    fn get_filtered_account_state_update(
        &self,
        keys: Vec<(&AccountStateIdType, &AccountStateKeyType)>,
    ) -> HashMap<(AccountStateIdType, AccountStateKeyType), AccountStateValueType>;

    #[allow(clippy::mutable_key_type)]
    fn get_filtered_component_balance_update(
        &self,
        keys: Vec<(&String, &Bytes)>,
    ) -> HashMap<(String, Bytes), ComponentBalance>;

    #[allow(clippy::mutable_key_type)]
    fn get_filtered_account_balance_update(
        &self,
        keys: Vec<(&Address, &Address)>,
    ) -> HashMap<(Address, Address), AccountBalance>;
}

impl<B> ReorgBuffer<B>
where
    B: BlockScoped + StateUpdateBufferEntry + DeepSizeOf,
{
    /// Looks up buffered protocol state updates for the provided keys. Returns a map of updates and
    /// a list of keys for which updates were not found in the buffered blocks.
    // Clippy thinks it is a complex type that is difficult to read
    #[allow(clippy::type_complexity)]
    pub fn lookup_protocol_state(
        &self,
        keys: &[(&ProtocolStateIdType, &ProtocolStateKeyType)],
    ) -> (
        HashMap<(ProtocolStateIdType, ProtocolStateKeyType), ProtocolStateValueType>,
        Vec<(ProtocolStateIdType, ProtocolStateKeyType)>,
    ) {
        let mut res = HashMap::new();
        let mut remaining_keys: HashSet<(ProtocolStateIdType, ProtocolStateKeyType)> =
            HashSet::from_iter(
                keys.iter()
                    .map(|&(c_id, attr)| (c_id.clone(), attr.clone())),
            );

        for block_message in self.block_messages.iter().rev() {
            if remaining_keys.is_empty() {
                break;
            }

            for (key, val) in block_message.get_filtered_protocol_state_update(
                remaining_keys
                    .iter()
                    .map(|k| (&k.0, &k.1))
                    .collect(),
            ) {
                if remaining_keys.remove(&(key.0.clone(), key.1.clone())) {
                    res.insert(key, val);
                }
            }
        }

        (res, remaining_keys.into_iter().collect())
    }

    /// Looks up buffered account state updates for the provided keys. Returns a map of updates and
    /// a list of keys for which updates were not found in the buffered blocks.
    #[allow(clippy::mutable_key_type, clippy::type_complexity)]
    pub fn lookup_account_state(
        &self,
        keys: &[(&AccountStateIdType, &AccountStateKeyType)],
    ) -> (
        HashMap<(AccountStateIdType, AccountStateKeyType), AccountStateValueType>,
        Vec<(AccountStateIdType, AccountStateKeyType)>,
    ) {
        let mut res = HashMap::new();
        let mut remaining_keys: HashSet<(AccountStateIdType, AccountStateKeyType)> =
            HashSet::from_iter(
                keys.iter()
                    .map(|&(c_id, attr)| (c_id.clone(), attr.clone())),
            );

        for block_message in self.block_messages.iter().rev() {
            if remaining_keys.is_empty() {
                break;
            }

            for (key, val) in block_message.get_filtered_account_state_update(
                remaining_keys
                    .iter()
                    .map(|k| (&k.0, &k.1))
                    .collect(),
            ) {
                if remaining_keys.remove(&(key.0.clone(), key.1.clone())) {
                    res.insert(key, val);
                }
            }
        }

        (res, remaining_keys.into_iter().collect())
    }

    /// Looks up buffered balance updates for the provided component and token keys. Returns a map
    /// where each key is a component ID associated with its token balances, and a list of
    /// component-token pairs for which no updates were found.
    #[allow(clippy::type_complexity, clippy::mutable_key_type)]
    pub fn lookup_component_balances(
        &self,
        keys: &[(&ComponentId, &Bytes)],
    ) -> (HashMap<String, HashMap<Bytes, ComponentBalance>>, Vec<(ComponentId, Bytes)>) {
        let mut res = HashMap::new();
        let mut remaning_keys: HashSet<_> = keys
            .iter()
            .map(|(c_id, token)| (c_id.to_string(), token.to_owned().to_owned()))
            .collect();

        for block_message in self.block_messages.iter().rev() {
            if remaning_keys.is_empty() {
                break;
            }
            for (key, val) in
                block_message.get_filtered_component_balance_update(keys.to_vec().clone())
            {
                if remaning_keys.remove(&key) {
                    res.entry(key).or_insert(val);
                }
            }
        }

        let mut results: HashMap<String, HashMap<Bytes, ComponentBalance>> = HashMap::new();
        for ((component_id, address), val) in res {
            results
                .entry(component_id)
                .or_default()
                .insert(address, val);
        }

        (results, remaning_keys.into_iter().collect())
    }

    /// Looks up buffered account balance updates for the provided account and token keys. Returns a
    /// map where each key is an account address associated with its token balances, and a list
    /// of account-token pairs for which no updates were found.
    #[allow(clippy::type_complexity, clippy::mutable_key_type)]
    pub fn lookup_account_balances(
        &self,
        keys: &[(&Address, &Address)],
    ) -> (HashMap<Address, HashMap<Address, AccountBalance>>, Vec<(Address, Address)>) {
        let mut res = HashMap::new();
        let mut remaning_keys = keys
            .iter()
            .map(|(account, token)| (account.to_owned().to_owned(), token.to_owned().to_owned()))
            .collect::<HashSet<_>>();

        for block_message in self.block_messages.iter().rev() {
            if remaning_keys.is_empty() {
                break;
            }
            for (key, val) in
                block_message.get_filtered_account_balance_update(keys.to_vec().clone())
            {
                if remaning_keys.remove(&key) {
                    res.entry(key).or_insert(val);
                }
            }
        }

        let mut results: HashMap<Address, HashMap<Address, AccountBalance>> = HashMap::new();
        for ((account, token), val) in res {
            results
                .entry(account)
                .or_default()
                .insert(token, val);
        }

        (results, remaning_keys.into_iter().collect())
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::models::{
        blockchain::{Transaction, TxWithChanges},
        protocol::ProtocolComponentStateDelta,
        Chain,
    };

    use super::*;
    use crate::{extractor::models::BlockChanges, testing};

    fn transaction() -> Transaction {
        Transaction::new(
            Bytes::zero(32),
            Bytes::zero(32),
            Bytes::zero(20),
            Some(Bytes::zero(20)),
            10,
        )
    }

    const VM_CONTRACT_1: &str = "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688";
    const VM_CONTRACT_2: &str = "0xbbbbbbbbb24eeeb8d57d431224f73832bc34f688";

    const DAI_ADDRESS: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";

    fn get_block_changes(version: u8) -> BlockChanges {
        let token_addr = Bytes::from_str(DAI_ADDRESS).unwrap();
        let account1_addr = Bytes::from_str(VM_CONTRACT_1).unwrap();
        let account2_addr = Bytes::from_str(VM_CONTRACT_2).unwrap();
        match version {
            1 => {
                let tx = transaction();
                let attr = HashMap::from([
                    ("new".to_owned(), Bytes::from(1_u64.to_be_bytes().to_vec())),
                    ("reserve".to_owned(), Bytes::from(10_u64.to_be_bytes().to_vec())),
                ]);
                let state_updates = HashMap::from([(
                    "State1".to_owned(),
                    ProtocolComponentStateDelta {
                        component_id: "State1".to_owned(),
                        updated_attributes: attr,
                        deleted_attributes: HashSet::new(),
                        ..Default::default()
                    },
                )]);
                let component_balances = HashMap::from([
                    (
                        "Balance1".to_string(),
                        [(
                            token_addr.clone(),
                            ComponentBalance {
                                token: token_addr.clone(),
                                balance: Bytes::from(1_i32.to_be_bytes()),
                                modify_tx: tx.hash.clone(),
                                component_id: "Balance1".to_string(),
                                balance_float: 1.0,
                            },
                        )]
                        .into_iter()
                        .collect(),
                    ),
                    (
                        "Balance2".to_string(),
                        [(
                            Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
                            ComponentBalance {
                                token: token_addr.clone(),
                                balance: Bytes::from(30_i32.to_be_bytes()),
                                modify_tx: tx.hash.clone(),
                                component_id: "Balance2".to_string(),
                                balance_float: 30.0,
                            },
                        )]
                        .into_iter()
                        .collect(),
                    ),
                ]);

                BlockChanges::new(
                    "test".to_string(),
                    Chain::Ethereum,
                    testing::block(1),
                    0,
                    false,
                    vec![TxWithChanges {
                        state_updates,
                        balance_changes: component_balances,
                        tx,
                        ..Default::default()
                    }],
                    Vec::new(),
                )
            }
            2 => {
                let tx = transaction();
                let state_updates = HashMap::from([
                    (
                        "State1".to_owned(),
                        ProtocolComponentStateDelta {
                            component_id: "State1".to_owned(),
                            updated_attributes: HashMap::from([(
                                "new".to_owned(),
                                Bytes::from(2_u64.to_be_bytes().to_vec()),
                            )]),
                            deleted_attributes: HashSet::new(),
                            ..Default::default()
                        },
                    ),
                    (
                        "State2".to_owned(),
                        ProtocolComponentStateDelta {
                            component_id: "State2".to_owned(),
                            updated_attributes: HashMap::from([
                                ("new".to_owned(), Bytes::from(3_u64.to_be_bytes().to_vec())),
                                ("reserve".to_owned(), Bytes::from(30_u64.to_be_bytes().to_vec())),
                            ]),
                            deleted_attributes: HashSet::new(),
                            ..Default::default()
                        },
                    ),
                ]);
                let account_balances = HashMap::from([
                    (
                        account1_addr.clone(),
                        [(
                            token_addr.clone(),
                            AccountBalance {
                                token: token_addr.clone(),
                                balance: Bytes::from(2_i32.to_be_bytes()),
                                modify_tx: tx.hash.clone(),
                                account: account1_addr.clone(),
                            },
                        )]
                        .into_iter()
                        .collect(),
                    ),
                    (
                        account2_addr.clone(),
                        [(
                            token_addr.clone(),
                            AccountBalance {
                                token: token_addr.clone(),
                                balance: Bytes::from(30_i32.to_be_bytes()),
                                modify_tx: tx.hash.clone(),
                                account: account2_addr,
                            },
                        )]
                        .into_iter()
                        .collect(),
                    ),
                ]);

                BlockChanges::new(
                    "test".to_string(),
                    Chain::Ethereum,
                    testing::block(2),
                    0,
                    false,
                    vec![TxWithChanges {
                        state_updates,
                        account_balance_changes: account_balances,
                        tx,
                        ..Default::default()
                    }],
                    Vec::new(),
                )
            }
            3 => {
                let tx = transaction();
                let component_balances = HashMap::from([(
                    "Balance1".to_string(),
                    [(
                        token_addr.clone(),
                        ComponentBalance {
                            token: token_addr.clone(),
                            balance: Bytes::from(3_i32.to_be_bytes()),
                            modify_tx: tx.hash.clone(),
                            component_id: "Balance1".to_string(),
                            balance_float: 3.0,
                        },
                    )]
                    .into_iter()
                    .collect(),
                )]);
                let account_balances = HashMap::from([(
                    account1_addr.clone(),
                    [(
                        token_addr.clone(),
                        AccountBalance {
                            token: token_addr,
                            balance: Bytes::from(100_i32.to_be_bytes()),
                            modify_tx: tx.hash.clone(),
                            account: account1_addr,
                        },
                    )]
                    .into_iter()
                    .collect(),
                )]);

                BlockChanges::new(
                    "test".to_string(),
                    Chain::Ethereum,
                    testing::block(3),
                    0,
                    false,
                    vec![TxWithChanges {
                        tx,
                        balance_changes: component_balances,
                        account_balance_changes: account_balances,
                        ..Default::default()
                    }],
                    Vec::new(),
                )
            }
            _ => panic!("block entity version not implemented"),
        }
    }
    #[test]
    fn test_reorg_buffer_state_lookup() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();

        let c_ids = ["State1".to_string(), "State2".to_string()];
        let new = "new".to_string();
        let reserve = "reserve".to_string();
        let missing = "missing".to_string();

        let keys = vec![
            (&c_ids[0], &new),
            (&c_ids[0], &reserve),
            (&c_ids[1], &reserve),
            (&missing, &new),
            (&c_ids[0], &missing),
        ];

        let (res, mut missing_keys) = reorg_buffer.lookup_protocol_state(&keys);

        // Need to sort because collecting a HashSet is unstable.
        missing_keys.sort();
        assert_eq!(
            missing_keys,
            vec![(c_ids[0].clone(), missing.clone()), (missing.clone(), new.clone())]
        );
        assert_eq!(
            res,
            HashMap::from([
                ((c_ids[0].clone(), new.clone()), Bytes::from(2_u64.to_be_bytes().to_vec())),
                ((c_ids[0].clone(), reserve.clone()), Bytes::from(10_u64.to_be_bytes().to_vec())),
                ((c_ids[1].clone(), reserve.clone()), Bytes::from(30_u64.to_be_bytes().to_vec()))
            ])
        );
    }

    #[test]
    fn test_component_balance_lookup() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        let c_ids = ["Balance1".to_string(), "Balance2".to_string()];
        let token_key = Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let missing_token = Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let missing_component = "missing".to_string();

        let keys = vec![
            (&c_ids[0], &token_key),
            (&c_ids[1], &token_key),
            (&c_ids[1], &missing_token),
            (&missing_component, &token_key),
        ];

        let (res, mut missing_keys) = reorg_buffer.lookup_component_balances(&keys);

        // Need to sort because collecting a HashSet is unstable.
        missing_keys.sort();
        assert_eq!(
            missing_keys,
            vec![
                (c_ids[1].clone(), missing_token.clone()),
                (missing_component.clone(), token_key.clone())
            ]
        );
        assert_eq!(
            res,
            HashMap::from([
                (
                    c_ids[0].clone(),
                    HashMap::from([(
                        token_key.clone(),
                        tycho_common::models::protocol::ComponentBalance {
                            token: token_key.clone(),
                            balance: Bytes::from(3_i32.to_be_bytes()),
                            modify_tx: transaction().hash,
                            component_id: c_ids[0].clone(),
                            balance_float: 3.0,
                        }
                    )])
                ),
                (
                    c_ids[1].clone(),
                    HashMap::from([(
                        token_key.clone(),
                        tycho_common::models::protocol::ComponentBalance {
                            token: token_key.clone(),
                            balance: Bytes::from(30_i32.to_be_bytes()),
                            modify_tx: transaction().hash,
                            component_id: c_ids[1].clone(),
                            balance_float: 30.0,
                        }
                    )])
                )
            ])
        );
    }

    #[test]
    fn test_account_balance_lookup() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        let accounts =
            [Bytes::from_str(VM_CONTRACT_1).unwrap(), Bytes::from_str(VM_CONTRACT_2).unwrap()];
        let token_key = Bytes::from_str(DAI_ADDRESS).unwrap();
        let missing_token = Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let missing_account =
            Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap();

        let keys = vec![
            (&accounts[0], &token_key),
            (&accounts[1], &token_key),
            (&accounts[1], &missing_token),
            (&missing_account, &token_key),
        ];

        let (res, mut missing_keys) = reorg_buffer.lookup_account_balances(&keys);

        // Need to sort because collecting a HashSet is unstable.
        missing_keys.sort();
        assert_eq!(
            missing_keys,
            vec![
                (missing_account.clone(), token_key.clone()),
                (accounts[1].clone(), missing_token.clone())
            ]
        );
        assert_eq!(
            res,
            HashMap::from([
                (
                    accounts[0].clone(),
                    HashMap::from([(
                        token_key.clone(),
                        AccountBalance {
                            token: token_key.clone(),
                            balance: Bytes::from(100_i32.to_be_bytes()),
                            modify_tx: transaction().hash,
                            account: accounts[0].clone(),
                        }
                    )])
                ),
                (
                    accounts[1].clone(),
                    HashMap::from([(
                        token_key.clone(),
                        AccountBalance {
                            token: token_key,
                            balance: Bytes::from(30_i32.to_be_bytes()),
                            modify_tx: transaction().hash,
                            account: accounts[1].clone(),
                        }
                    )])
                )
            ])
        );
    }

    #[test]
    fn test_count_blocks_before() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        assert_eq!(reorg_buffer.count_blocks_before(1), 0);
        assert_eq!(reorg_buffer.count_blocks_before(2), 1);
        assert_eq!(reorg_buffer.count_blocks_before(3), 2);
        assert_eq!(reorg_buffer.count_blocks_before(4), 3);
    }

    #[test]
    fn test_drain_committed_blocks() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer.strict = true;
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        let committed = reorg_buffer
            .drain_blocks_until(3)
            .unwrap();

        assert_eq!(reorg_buffer.block_messages.len(), 1);
        assert_eq!(committed, vec![get_block_changes(1), get_block_changes(2)]);

        let unknown = reorg_buffer.drain_blocks_until(999);

        assert!(unknown.is_err());
    }

    #[test]
    fn test_purge() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        let purged = reorg_buffer
            .purge(
                Bytes::from_str(
                    "0x0000000000000000000000000000000000000000000000000000000000000001",
                )
                .unwrap(),
            )
            .unwrap();

        assert_eq!(reorg_buffer.block_messages.len(), 1);

        assert_eq!(purged, vec![get_block_changes(2), get_block_changes(3)]);

        let unknown = reorg_buffer.purge(
            Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000999")
                .unwrap(),
        );

        assert!(unknown.is_err());
    }

    #[test]
    #[should_panic]
    fn test_insert_wrong_block() {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();
    }

    #[rstest]
    #[case::complete_range(None, None, vec![1, 2, 3])]
    #[case::range(Some("2020-01-01T00:00:12".parse::<NaiveDateTime>().unwrap()), Some("2020-01-01T00:00:24".parse::<NaiveDateTime>().unwrap()), vec![1, 2])]
    #[case::from_start(None, Some("2020-01-01T00:00:24".parse::<NaiveDateTime>().unwrap()), vec![1, 2])]
    #[case::until_end(Some("2020-01-01T00:00:24".parse::<NaiveDateTime>().unwrap()), None, vec![2, 3])]
    fn test_get_block_range(
        #[case] start: Option<NaiveDateTime>,
        #[case] end: Option<NaiveDateTime>,
        #[case] exp: Vec<u64>,
    ) {
        let start = start.map(BlockNumberOrTimestamp::Timestamp);
        let end = end.map(BlockNumberOrTimestamp::Timestamp);
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        let blocks = reorg_buffer
            .get_block_range(start, end)
            .unwrap()
            .map(|e| e.block.number)
            .collect::<Vec<_>>();

        assert_eq!(blocks, exp);
    }

    #[test]
    fn test_get_block_range_empty() {
        let reorg_buffer = ReorgBuffer::<BlockChanges>::new();

        let res = reorg_buffer
            .get_block_range(None, None)
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(res, Vec::<&BlockChanges>::new());
    }

    #[rstest]
    #[case::beyond_range(Some("2020-01-01T00:00:12".parse::<NaiveDateTime>().unwrap()), Some("2020-01-01T00:00:36".parse::<NaiveDateTime>().unwrap()), Ok(vec![1, 2]))]
    #[case::invalid(Some("2020-01-01T00:00:24".parse::<NaiveDateTime>().unwrap()), Some("2020-01-01T00:00:12".parse::<NaiveDateTime>().unwrap()), Err(StorageError::Unexpected("ReorgBuffer: Invalid block range".to_string())))]
    fn test_get_block_range_edge_cases(
        #[case] start: Option<NaiveDateTime>,
        #[case] end: Option<NaiveDateTime>,
        #[case] exp: Result<Vec<u64>, StorageError>,
    ) {
        let start = start.map(BlockNumberOrTimestamp::Timestamp);
        let end = end.map(BlockNumberOrTimestamp::Timestamp);
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();

        let res = reorg_buffer
            .get_block_range(start, end)
            .map(|iter| {
                iter.map(|b| b.block().number)
                    .collect::<Vec<_>>()
            });

        assert_eq!(res, exp);
    }

    #[rstest]
    #[case::committed_no(BlockNumberOrTimestamp::Number(0), CommitStatus::Committed)]
    #[case::committed_ts(
        BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap()),
        CommitStatus::Committed
    )]
    #[case::uncommitted_no(BlockNumberOrTimestamp::Number(2), CommitStatus::Uncommitted)]
    #[case::uncommitted_ts(
        BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:15".parse().unwrap()), 
        CommitStatus::Uncommitted
    )]
    #[case::unseen_no(BlockNumberOrTimestamp::Number(5), CommitStatus::Unseen)]
    #[case::unseen_ts(
        BlockNumberOrTimestamp::Timestamp("2020-01-01T01:00:00".parse().unwrap()),
        CommitStatus::Unseen
    )]
    fn test_get_commit_status(#[case] version: BlockNumberOrTimestamp, #[case] exp: CommitStatus) {
        let mut reorg_buffer = ReorgBuffer::new();
        reorg_buffer
            .insert_block(get_block_changes(1))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(2))
            .unwrap();
        reorg_buffer
            .insert_block(get_block_changes(3))
            .unwrap();

        let res = reorg_buffer.get_commit_status(version);

        assert_eq!(res, Some(exp));
    }

    #[test]
    fn test_get_commit_status_empty() {
        let reorg_buffer = ReorgBuffer::<BlockChanges>::new();

        let res = reorg_buffer.get_commit_status(BlockNumberOrTimestamp::Number(2));

        assert_eq!(res, None);
    }
}
