//! Elysium values the launchpad modules' tests share.

use std::collections::BTreeSet;

use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::{
    Block, Log, TransactionReceipt, TransactionTrace, TransactionTraceStatus,
};

use crate::{
    address_list::Address,
    launchpad::params::Params,
    storage::{pad32, Word},
};

/// The Elysium Uniswap V4 `PoolManager`.
pub(crate) const POOL_MANAGER: Address = hex!("e2e105d3f7209a9da11f83ccf3e7398a753823f1");
/// The Montra `LaunchHook`.
pub(crate) const HOOK: Address = hex!("6b1216a40b65c83e9c9e18a562c446697b8cf8ec");
/// The Ascend launchpad hook.
pub(crate) const ASCEND_HOOK: Address = hex!("e0def4221b9840a610d4e7d55812a79a6431a8ec");
/// Pool 0x35bd0e6a..., launched in tx 0xe6956588... at block 1029110, native HYPE against
/// [`TOKEN`].
pub(crate) const POOL_ID: Word =
    hex!("35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2");
/// The launch token of [`POOL_ID`].
pub(crate) const TOKEN: Address = hex!("32193afd14c53f12c45a4c04eca9f7ba98e3ec71");
/// The transaction that launched [`POOL_ID`].
pub(crate) const LAUNCH_TX: [u8; 32] =
    hex!("e69565883dcf7ff890325eb1bf34bed5c281be88e89ffebdbdddd9837134a255");

/// Ascend pool 0xa138ea3b... of [`ASCEND_HOOK`].
pub(crate) const ASCEND_POOL_ID: Word =
    hex!("a138ea3bc220ee819a81be441ad1fd95f84e5ff228a6c1d862376c66e60a01a5");
/// [`ASCEND_POOL_ID`]'s component id.
pub(crate) const ASCEND_POOL: &str =
    "0xa138ea3bc220ee819a81be441ad1fd95f84e5ff228a6c1d862376c66e60a01a5";
/// [`ASCEND_POOL_ID`]'s `PoolData` word 2 at block 8365800 (`cast storage`): fee 10000
/// (0x002710), startTick -168600 (0xfd6d68), launchedAt 1794307976 (0x6af2f788), tokenIs0 0, then
/// the creator, whose bytes here are arbitrary.
pub(crate) const ASCEND_TERMS_WORD: Word =
    hex!("002710fd6d68006af2f78800aabbccddeeff00112233445566778899aabbccdd");
/// A `PoolData` word 3 as a launch writes it: d = 0, observedTick = start - 200 = -400800
/// (0xf9e260) for a pool starting at -400600, emaUpdatedAt = 1794362659 (0x6afecd23), and an
/// arbitrary 20-byte EMA.
pub(crate) const ASCEND_LAUNCH_OBSERVATION_WORD: Word =
    hex!("00000000f9e260006afecd2300000000000000000883956ab34ccb0338ab12cd");

const INITIALIZE_TOPIC: Word =
    hex!("dd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438");

/// The Elysium manifest's parameters: [`POOL_MANAGER`], [`HOOK`] and [`ASCEND_HOOK`].
pub(crate) fn params() -> Params {
    params_with_montra_hooks(&[HOOK])
}

/// The Elysium manifest's parameters with `montra_hooks` as the Montra hooks.
pub(crate) fn params_with_montra_hooks(montra_hooks: &[Address]) -> Params {
    Params::try_new(
        POOL_MANAGER,
        montra_hooks.iter().copied().collect(),
        BTreeSet::from([ASCEND_HOOK]),
    )
    .expect("the fixture hooks are disjoint")
}

fn word(bytes: &[u8]) -> Word {
    pad32(bytes).expect("test values fit a word")
}

/// The `Initialize` log of [`POOL_ID`] (receipt of [`LAUNCH_TX`]), with its emitter and hook
/// replaceable: native HYPE and [`TOKEN`], fee 0x800000, tick spacing 200, sqrtPriceX96 and tick
/// at the 169800 opening.
pub(crate) fn initialize_log(emitter: &Address, hooks: &Address) -> Log {
    let mut data = Vec::new();
    data.extend(word(&hex!("800000")));
    data.extend(word(&hex!("c8")));
    data.extend(word(hooks));
    data.extend(word(&hex!("12ffcd142cc3f684e7ac97071c0a")));
    data.extend(word(&hex!("029748")));
    Log {
        address: emitter.to_vec(),
        topics: vec![
            INITIALIZE_TOPIC.to_vec(),
            POOL_ID.to_vec(),
            word(&[0u8; 20]).to_vec(),
            word(&TOKEN).to_vec(),
        ],
        data,
        ..Default::default()
    }
}

/// Block 1029110 holding [`LAUNCH_TX`] at index 1, with `logs` as its receipt's logs.
pub(crate) fn launch_block(logs: Vec<Log>) -> Block {
    Block {
        number: 1029110,
        transaction_traces: vec![TransactionTrace {
            hash: LAUNCH_TX.to_vec(),
            index: 1,
            status: i32::from(TransactionTraceStatus::Succeeded),
            receipt: Some(TransactionReceipt { logs, ..Default::default() }),
            ..Default::default()
        }],
        ..Default::default()
    }
}
