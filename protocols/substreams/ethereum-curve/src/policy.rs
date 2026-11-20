//! Follows the policy contract of a Twocrypto pool, and the grant terms of a policy that announces
//! itself with `FyndPolicyCreated`.

use substreams::Hex;
use substreams_ethereum::{
    pb::eth::v2::{CallType, Log, TransactionTrace},
    Event,
};
use tycho_substreams::prelude::*;

use crate::{
    abi::{
        fynd_policy::events::{FyndPolicyCreated, SetController, SetKilled, SetMinFee, SetRouter},
        twocrypto_pool::events::SetPolicyContract,
    },
    consts::ZERO_ADDRESS,
};

/// Name of the state attribute that holds the pool's policy contract.
pub const POLICY_ATTRIBUTE: &str = "policy";
/// Name of the state attribute that holds the version of the policy's grant rules.
pub const POLICY_VERSION_ATTRIBUTE: &str = "policy_version";
/// Name of the state attribute that holds the key that signs the policy's grants.
pub const POLICY_CONTROLLER_ATTRIBUTE: &str = "policy_controller";
/// Name of the state attribute that holds the only caller that can submit a grant.
pub const POLICY_ROUTER_ATTRIBUTE: &str = "policy_router";
/// Name of the state attribute that is `1` while the policy refuses grants.
pub const POLICY_KILLED_ATTRIBUTE: &str = "policy_killed";
/// Name of the state attribute that holds the lowest fee of a granted swap, in units of 1e-10.
pub const POLICY_MIN_FEE_ATTRIBUTE: &str = "policy_min_fee";

/// The grant terms that the policy emits after its creation, with their unset values.
const GRANT_TERM_ATTRIBUTES: [(&str, &[u8]); 4] = [
    (POLICY_CONTROLLER_ATTRIBUTE, &ZERO_ADDRESS),
    (POLICY_ROUTER_ATTRIBUTE, &ZERO_ADDRESS),
    (POLICY_KILLED_ATTRIBUTE, &[0]),
    (POLICY_MIN_FEE_ATTRIBUTE, &[0]),
];

/// The pool that a grant policy was created for, and the version of its grant rules.
#[derive(Debug, PartialEq)]
pub struct PolicyGrant {
    pub pool: Vec<u8>,
    pub version: Vec<u8>,
}

impl PolicyGrant {
    /// Encodes the grant for the created policy store.
    pub fn encode(&self) -> Vec<u8> {
        [self.pool.as_slice(), &self.version].concat()
    }

    /// Decodes a value of the created policy store.
    pub fn decode(value: &[u8]) -> Option<Self> {
        (value.len() > 20).then(|| {
            let (pool, version) = value.split_at(20);
            Self { pool: pool.to_vec(), version: version.to_vec() }
        })
    }
}

/// A grant policy that a transaction creates.
#[derive(Debug, PartialEq)]
pub struct CreatedPolicy {
    pub ordinal: u64,
    pub policy: Vec<u8>,
    pub grant: PolicyGrant,
}

/// A value that a transaction writes to the policy state store.
#[derive(Debug, PartialEq)]
pub struct PolicyStateWrite {
    pub ordinal: u64,
    pub key: String,
    pub value: Vec<u8>,
}

/// Key of a pool's policy contract in the policy state store.
pub fn pool_policy_key(pool: &[u8]) -> String {
    format!("pool:{}", Hex(pool))
}

/// Key of a grant term of a policy in the policy state store.
pub fn grant_term_key(policy: &[u8], attribute: &str) -> String {
    format!("{}:{attribute}", Hex(policy))
}

/// Key of a grant policy in the created policy store.
pub fn created_policy_key(policy: &[u8]) -> String {
    Hex(policy).to_string()
}

/// The policy attributes of a new Twocrypto pool, before it sets a policy.
pub fn initial_policy_attributes() -> Vec<Attribute> {
    let mut attributes =
        vec![attribute(POLICY_ATTRIBUTE, ZERO_ADDRESS.to_vec(), ChangeType::Creation)];
    attributes.extend(zero_grant_attributes(ChangeType::Creation));
    attributes
}

/// Returns the grant policies that `tx` creates. A grant policy emits `FyndPolicyCreated` in its
/// constructor.
pub fn created_policies(tx: &TransactionTrace) -> Vec<CreatedPolicy> {
    let mut policies = vec![];
    for call in &tx.calls {
        if call.call_type() != CallType::Create || call.state_reverted {
            continue;
        }
        for log in &call.logs {
            if log.address != call.address {
                continue;
            }
            let Some(event) = FyndPolicyCreated::match_and_decode(log) else {
                continue;
            };
            policies.push(CreatedPolicy {
                ordinal: log.ordinal,
                policy: log.address.clone(),
                grant: PolicyGrant { pool: event.pool, version: uint_bytes(&event.version) },
            });
        }
    }
    policies
}

/// Returns the policy state that `tx` changes: the policy of each tracked pool, and the grant
/// terms of each grant policy.
pub fn policy_state_writes(
    tx: &TransactionTrace,
    is_tracked_pool: impl Fn(&[u8]) -> bool,
    is_grant_policy: impl Fn(&[u8]) -> bool,
) -> Vec<PolicyStateWrite> {
    let Some(receipt) = tx.receipt.as_ref() else {
        return vec![];
    };
    let mut writes = vec![];
    for log in &receipt.logs {
        if let Some(event) = SetPolicyContract::match_and_decode(log) {
            if is_tracked_pool(&log.address) {
                writes.push(PolicyStateWrite {
                    ordinal: log.ordinal,
                    key: pool_policy_key(&log.address),
                    value: event.policy,
                });
            }
            continue;
        }
        let Some((name, value)) = decode_grant_term(log) else {
            continue;
        };
        if is_grant_policy(&log.address) {
            writes.push(PolicyStateWrite {
                ordinal: log.ordinal,
                key: grant_term_key(&log.address, name),
                value,
            });
        }
    }
    writes
}

/// Returns the policy attribute changes of each tracked pool in `tx`.
///
/// When a pool sets a policy, all its policy attributes change. A grant policy created for that
/// pool gives its version and grant terms; any other policy gives zero values. When the policy of
/// a pool changes a grant term, the matching attribute of the pool changes. A grant policy
/// created after its pool sets it gives its version when it is created.
///
/// `created_policy` returns the grant of a grant policy.
/// `policy_state_at` returns the value of the policy state store at an ordinal.
pub fn policy_entity_changes(
    tx: &TransactionTrace,
    is_tracked_pool: impl Fn(&[u8]) -> bool,
    created_policy: impl Fn(&[u8]) -> Option<PolicyGrant>,
    policy_state_at: impl Fn(u64, &str) -> Option<Vec<u8>>,
) -> Vec<EntityChanges> {
    let Some(receipt) = tx.receipt.as_ref() else {
        return vec![];
    };
    let mut changes: Vec<EntityChanges> = vec![];
    let is_attached = |pool: &[u8], policy: &[u8], ordinal: u64| {
        policy_state_at(ordinal, &pool_policy_key(pool)).is_some_and(|attached| attached == policy)
    };
    for log in &receipt.logs {
        if FyndPolicyCreated::match_and_decode(log).is_some() {
            let Some(PolicyGrant { pool, version }) = created_policy(&log.address) else {
                continue;
            };
            if is_tracked_pool(&pool) && is_attached(&pool, &log.address, log.ordinal) {
                let version = attribute(POLICY_VERSION_ATTRIBUTE, version, ChangeType::Update);
                merge_attributes(&mut changes, format_component_id(&pool), vec![version]);
            }
            continue;
        }
        if let Some(event) = SetPolicyContract::match_and_decode(log) {
            if !is_tracked_pool(&log.address) {
                continue;
            }
            let attributes = pool_policy_attributes(
                &log.address,
                &event.policy,
                log.ordinal,
                &created_policy,
                &policy_state_at,
            );
            merge_attributes(&mut changes, format_component_id(&log.address), attributes);
            continue;
        }
        let Some((name, value)) = decode_grant_term(log) else {
            continue;
        };
        let Some(PolicyGrant { pool, .. }) = created_policy(&log.address) else {
            continue;
        };
        if is_attached(&pool, &log.address, log.ordinal) {
            let term = attribute(name, value, ChangeType::Update);
            merge_attributes(&mut changes, format_component_id(&pool), vec![term]);
        }
    }
    changes
}

/// Returns all policy attributes of `pool` after it sets `policy`.
fn pool_policy_attributes(
    pool: &[u8],
    policy: &[u8],
    ordinal: u64,
    created_policy: impl Fn(&[u8]) -> Option<PolicyGrant>,
    policy_state_at: impl Fn(u64, &str) -> Option<Vec<u8>>,
) -> Vec<Attribute> {
    let mut attributes = vec![attribute(POLICY_ATTRIBUTE, policy.to_vec(), ChangeType::Update)];
    let version = created_policy(policy)
        .filter(|grant| grant.pool == pool)
        .map(|grant| grant.version);
    let Some(version) = version else {
        attributes.extend(zero_grant_attributes(ChangeType::Update));
        return attributes;
    };
    attributes.push(attribute(POLICY_VERSION_ATTRIBUTE, version, ChangeType::Update));
    for (name, unset) in GRANT_TERM_ATTRIBUTES {
        let value = policy_state_at(ordinal, &grant_term_key(policy, name))
            .unwrap_or_else(|| unset.to_vec());
        attributes.push(attribute(name, value, ChangeType::Update));
    }
    attributes
}

/// Decodes a log that changes a grant term into the attribute name and value.
fn decode_grant_term(log: &Log) -> Option<(&'static str, Vec<u8>)> {
    if let Some(event) = SetController::match_and_decode(log) {
        return Some((POLICY_CONTROLLER_ATTRIBUTE, event.controller));
    }
    if let Some(event) = SetRouter::match_and_decode(log) {
        return Some((POLICY_ROUTER_ATTRIBUTE, event.router));
    }
    if let Some(event) = SetKilled::match_and_decode(log) {
        return Some((POLICY_KILLED_ATTRIBUTE, vec![u8::from(event.is_killed)]));
    }
    if let Some(event) = SetMinFee::match_and_decode(log) {
        return Some((POLICY_MIN_FEE_ATTRIBUTE, uint_bytes(&event.min_fee)));
    }
    None
}

/// Adds `attributes` to the changes of `component_id`. A later value of an attribute replaces an
/// earlier one.
pub fn merge_attributes(
    changes: &mut Vec<EntityChanges>,
    component_id: String,
    attributes: Vec<Attribute>,
) {
    let index = match changes
        .iter()
        .position(|change| change.component_id == component_id)
    {
        Some(index) => index,
        None => {
            changes.push(EntityChanges { component_id, attributes: vec![] });
            changes.len() - 1
        }
    };
    let known = &mut changes[index].attributes;
    for attribute in attributes {
        match known
            .iter_mut()
            .find(|existing| existing.name == attribute.name)
        {
            Some(existing) => existing.value = attribute.value,
            None => known.push(attribute),
        }
    }
}

fn format_component_id(pool: &[u8]) -> String {
    format!("0x{}", Hex(pool))
}

fn zero_grant_attributes(change: ChangeType) -> Vec<Attribute> {
    let mut attributes = vec![attribute(POLICY_VERSION_ATTRIBUTE, vec![0], change)];
    for (name, unset) in GRANT_TERM_ATTRIBUTES {
        attributes.push(attribute(name, unset.to_vec(), change));
    }
    attributes
}

fn attribute(name: &str, value: Vec<u8>, change: ChangeType) -> Attribute {
    Attribute { name: name.to_string(), value, change: change.into() }
}

/// Big-endian bytes of an unsigned integer, with no leading zeros.
fn uint_bytes(value: &substreams::scalar::BigInt) -> Vec<u8> {
    value.to_bytes_be().1
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rstest::rstest;
    use substreams::hex;
    use substreams_ethereum::pb::eth::v2::{Call, TransactionReceipt};

    use super::*;

    const POOL: [u8; 20] = hex!("1111111111111111111111111111111111111111");
    const OTHER_POOL: [u8; 20] = hex!("2222222222222222222222222222222222222222");
    const UNTRACKED_POOL: [u8; 20] = hex!("3333333333333333333333333333333333333333");
    const POLICY: [u8; 20] = hex!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const OTHER_POLICY: [u8; 20] = hex!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    const CONTROLLER: [u8; 20] = hex!("cccccccccccccccccccccccccccccccccccccccc");
    const ROUTER: [u8; 20] = hex!("dddddddddddddddddddddddddddddddddddddddd");
    const SET_POLICY_CONTRACT_TOPIC: [u8; 32] =
        hex!("78e81206388ab3ee5e73c68b87da16f8cc8968c831d517adec68a516c75d09b0");
    const FYND_POLICY_CREATED_TOPIC: [u8; 32] =
        hex!("ecb6227ed87ebdda224096ae94927c8294f62dd254947c299b74616971dec70f");
    const SET_CONTROLLER_TOPIC: [u8; 32] =
        hex!("4ff638452bbf33c012645d18ae6f05515ff5f2d1dfb0cece8cbf018c60903f70");
    const SET_MIN_FEE_TOPIC: [u8; 32] =
        hex!("81d2c871f6984b6743a9b4546acc03a595a2e61e088fcabd73bf8dc839266b81");
    const SET_ROUTER_TOPIC: [u8; 32] =
        hex!("6de4326a8b9054d72d9dbab97d27bc4edffadee7d966f5af9cc4eafdaf8e5455");
    const SET_KILLED_TOPIC: [u8; 32] =
        hex!("f6a23ecf31263066d4cc1ac21837c1714818045ecc5c0dd89f279e0bd4b7ef87");

    fn word(value: &[u8]) -> Vec<u8> {
        let mut word = vec![0u8; 32 - value.len()];
        word.extend_from_slice(value);
        word
    }

    fn log(address: [u8; 20], topics: Vec<Vec<u8>>, data: Vec<u8>, ordinal: u64) -> Log {
        Log { address: address.to_vec(), topics, data, ordinal, ..Default::default() }
    }

    fn set_policy_log(pool: [u8; 20], policy: [u8; 20], ordinal: u64) -> Log {
        log(pool, vec![SET_POLICY_CONTRACT_TOPIC.to_vec()], word(&policy), ordinal)
    }

    fn created_log(policy: [u8; 20], pool: [u8; 20], ordinal: u64) -> Log {
        log(policy, vec![FYND_POLICY_CREATED_TOPIC.to_vec(), word(&pool)], word(&[1]), ordinal)
    }

    fn set_controller_log(policy: [u8; 20], controller: [u8; 20], ordinal: u64) -> Log {
        log(policy, vec![SET_CONTROLLER_TOPIC.to_vec()], word(&controller), ordinal)
    }

    fn set_min_fee_log(policy: [u8; 20], min_fee: &[u8], ordinal: u64) -> Log {
        log(policy, vec![SET_MIN_FEE_TOPIC.to_vec()], word(min_fee), ordinal)
    }

    fn tx(logs: Vec<Log>) -> TransactionTrace {
        TransactionTrace {
            receipt: Some(TransactionReceipt { logs, ..Default::default() }),
            ..Default::default()
        }
    }

    fn is_tracked_pool(pool: &[u8]) -> bool {
        pool != UNTRACKED_POOL
    }

    /// The created policy store: `POLICY` was created for `POOL`, at version 1.
    fn created_policy(policy: &[u8]) -> Option<PolicyGrant> {
        (policy == POLICY).then(|| PolicyGrant { pool: POOL.to_vec(), version: vec![1] })
    }

    /// The policy state store: the last value of a key written at or before an ordinal.
    fn policy_state(
        entries: &[(u64, String, Vec<u8>)],
    ) -> impl Fn(u64, &str) -> Option<Vec<u8>> + '_ {
        |ordinal, key| {
            entries
                .iter()
                .filter(|(written, known, _)| *written <= ordinal && known == key)
                .last()
                .map(|(_, _, value)| value.clone())
        }
    }

    fn attributes(changes: &[EntityChanges]) -> Vec<(String, HashMap<String, Vec<u8>>)> {
        changes
            .iter()
            .map(|change| {
                let attributes = change
                    .attributes
                    .iter()
                    .map(|attribute| (attribute.name.clone(), attribute.value.clone()))
                    .collect();
                (change.component_id.clone(), attributes)
            })
            .collect()
    }

    fn pool_id(pool: [u8; 20]) -> String {
        format!("0x{}", Hex(pool))
    }

    /// The grant terms of `POLICY`, written at ordinal 0.
    fn grant_terms() -> Vec<(u64, String, Vec<u8>)> {
        vec![
            (0, grant_term_key(&POLICY, POLICY_CONTROLLER_ATTRIBUTE), CONTROLLER.to_vec()),
            (0, grant_term_key(&POLICY, POLICY_ROUTER_ATTRIBUTE), ROUTER.to_vec()),
            (0, grant_term_key(&POLICY, POLICY_KILLED_ATTRIBUTE), vec![0]),
            (0, grant_term_key(&POLICY, POLICY_MIN_FEE_ATTRIBUTE), vec![0x01, 0x86, 0xa0]),
        ]
    }

    #[test]
    fn test_created_policies_needs_constructor_log() {
        let created = Call {
            call_type: CallType::Create.into(),
            address: POLICY.to_vec(),
            logs: vec![created_log(POLICY, POOL, 3)],
            ..Default::default()
        };
        let emitted_by_other = Call {
            call_type: CallType::Create.into(),
            address: OTHER_POLICY.to_vec(),
            logs: vec![created_log(POLICY, OTHER_POOL, 4)],
            ..Default::default()
        };
        let not_created = Call {
            call_type: CallType::Call.into(),
            address: OTHER_POLICY.to_vec(),
            logs: vec![created_log(OTHER_POLICY, OTHER_POOL, 5)],
            ..Default::default()
        };
        let reverted = Call {
            call_type: CallType::Create.into(),
            address: OTHER_POLICY.to_vec(),
            logs: vec![created_log(OTHER_POLICY, OTHER_POOL, 6)],
            state_reverted: true,
            ..Default::default()
        };
        let tx = TransactionTrace {
            calls: vec![created, emitted_by_other, not_created, reverted],
            ..Default::default()
        };

        assert_eq!(
            created_policies(&tx),
            vec![CreatedPolicy {
                ordinal: 3,
                policy: POLICY.to_vec(),
                grant: PolicyGrant { pool: POOL.to_vec(), version: vec![1] },
            }]
        );
    }

    #[test]
    fn test_policy_grant_encode_decode() {
        let grant = PolicyGrant { pool: POOL.to_vec(), version: vec![1] };

        assert_eq!(PolicyGrant::decode(&grant.encode()), Some(grant));
        assert_eq!(PolicyGrant::decode(&POOL), None);
    }

    #[test]
    fn test_policy_state_writes() {
        let tx = tx(vec![
            set_controller_log(POLICY, CONTROLLER, 1),
            set_controller_log(OTHER_POLICY, CONTROLLER, 2),
            set_policy_log(POOL, POLICY, 3),
            set_policy_log(UNTRACKED_POOL, POLICY, 4),
        ]);

        let writes = policy_state_writes(&tx, is_tracked_pool, |policy| policy == POLICY);

        assert_eq!(
            writes,
            vec![
                PolicyStateWrite {
                    ordinal: 1,
                    key: grant_term_key(&POLICY, POLICY_CONTROLLER_ATTRIBUTE),
                    value: CONTROLLER.to_vec(),
                },
                PolicyStateWrite {
                    ordinal: 3,
                    key: pool_policy_key(&POOL),
                    value: POLICY.to_vec()
                },
            ]
        );
    }

    #[test]
    fn test_policy_entity_changes_attaches_grant_policy() {
        let tx = tx(vec![set_policy_log(POOL, POLICY, 1)]);
        let state = grant_terms();

        let changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&state));

        let expected = HashMap::from([
            (POLICY_ATTRIBUTE.to_string(), POLICY.to_vec()),
            (POLICY_VERSION_ATTRIBUTE.to_string(), vec![1]),
            (POLICY_CONTROLLER_ATTRIBUTE.to_string(), CONTROLLER.to_vec()),
            (POLICY_ROUTER_ATTRIBUTE.to_string(), ROUTER.to_vec()),
            (POLICY_KILLED_ATTRIBUTE.to_string(), vec![0]),
            (POLICY_MIN_FEE_ATTRIBUTE.to_string(), vec![0x01, 0x86, 0xa0]),
        ]);
        assert_eq!(attributes(&changes), vec![(pool_id(POOL), expected)]);
    }

    #[test]
    fn test_policy_entity_changes_zeroes_grant_terms_of_other_policies() {
        // `OTHER_POOL` sets the grant policy of `POOL`, then a policy without grants.
        for (pool, policy) in [(OTHER_POOL, POLICY), (POOL, OTHER_POLICY)] {
            let tx = tx(vec![set_policy_log(pool, policy, 1)]);
            let state = grant_terms();

            let changes =
                policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&state));

            let expected = HashMap::from([
                (POLICY_ATTRIBUTE.to_string(), policy.to_vec()),
                (POLICY_VERSION_ATTRIBUTE.to_string(), vec![0]),
                (POLICY_CONTROLLER_ATTRIBUTE.to_string(), ZERO_ADDRESS.to_vec()),
                (POLICY_ROUTER_ATTRIBUTE.to_string(), ZERO_ADDRESS.to_vec()),
                (POLICY_KILLED_ATTRIBUTE.to_string(), vec![0]),
                (POLICY_MIN_FEE_ATTRIBUTE.to_string(), vec![0]),
            ]);
            assert_eq!(attributes(&changes), vec![(pool_id(pool), expected)]);
        }
    }

    #[test]
    fn test_policy_entity_changes_follows_grant_terms_of_attached_policy() {
        let tx = tx(vec![set_min_fee_log(POLICY, &[0x02], 1), set_min_fee_log(POLICY, &[0x03], 2)]);
        let attached = vec![(0, pool_policy_key(&POOL), POLICY.to_vec())];
        let detached = vec![(0, pool_policy_key(&POOL), OTHER_POLICY.to_vec())];

        let changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&attached));
        let detached_changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&detached));

        let expected = HashMap::from([(POLICY_MIN_FEE_ATTRIBUTE.to_string(), vec![0x03])]);
        assert_eq!(attributes(&changes), vec![(pool_id(POOL), expected)]);
        assert_eq!(attributes(&detached_changes), vec![]);
    }

    #[test]
    fn test_policy_entity_changes_untracked_pool() {
        let tx = tx(vec![set_policy_log(UNTRACKED_POOL, POLICY, 1)]);

        let changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&[]));

        assert_eq!(attributes(&changes), vec![]);
    }

    #[rstest]
    #[case::router(SET_ROUTER_TOPIC, word(&ROUTER), POLICY_ROUTER_ATTRIBUTE, ROUTER.to_vec())]
    #[case::killed(SET_KILLED_TOPIC, word(&[1]), POLICY_KILLED_ATTRIBUTE, vec![1])]
    #[case::alive(SET_KILLED_TOPIC, word(&[0]), POLICY_KILLED_ATTRIBUTE, vec![0])]
    fn test_decode_grant_term(
        #[case] topic: [u8; 32],
        #[case] data: Vec<u8>,
        #[case] name: &str,
        #[case] value: Vec<u8>,
    ) {
        let log = log(POLICY, vec![topic.to_vec()], data, 1);

        assert_eq!(decode_grant_term(&log), Some((name, value)));
    }

    #[test]
    fn test_policy_entity_changes_attach_reads_terms_at_its_ordinal() {
        let tx = tx(vec![set_policy_log(POOL, POLICY, 2)]);
        let mut state = grant_terms();
        state.push((1, grant_term_key(&POLICY, POLICY_MIN_FEE_ATTRIBUTE), vec![0x02]));
        state.push((3, grant_term_key(&POLICY, POLICY_MIN_FEE_ATTRIBUTE), vec![0x03]));

        let changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&state));

        assert_eq!(attributes(&changes)[0].1[POLICY_MIN_FEE_ATTRIBUTE], vec![0x02]);
    }

    #[test]
    fn test_policy_entity_changes_grant_term_before_attach() {
        let tx = tx(vec![set_min_fee_log(POLICY, &[0x02], 1)]);
        let state = vec![(2, pool_policy_key(&POOL), POLICY.to_vec())];

        let changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&state));

        assert_eq!(attributes(&changes), vec![]);
    }

    #[rstest]
    #[case::attached(POLICY, Some(vec![1]))]
    #[case::other_policy_attached(OTHER_POLICY, None)]
    fn test_policy_entity_changes_policy_created_after_attach(
        #[case] attached: [u8; 20],
        #[case] version: Option<Vec<u8>>,
    ) {
        let tx = tx(vec![created_log(POLICY, POOL, 2)]);
        let state = vec![(1, pool_policy_key(&POOL), attached.to_vec())];

        let changes =
            policy_entity_changes(&tx, is_tracked_pool, created_policy, policy_state(&state));

        let expected = version
            .map(|version| {
                let attributes = HashMap::from([(POLICY_VERSION_ATTRIBUTE.to_string(), version)]);
                vec![(pool_id(POOL), attributes)]
            })
            .unwrap_or_default();
        assert_eq!(attributes(&changes), expected);
    }
}
