use substreams::hex;

/// One component for the whole venue: stETH mints, and wstETH wraps, unwraps and mints through
/// `receive()`. Keyed by stETH, the contract that holds the pool.
pub const STETH_COMPONENT_ID: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";

pub const STETH_ADDRESS: [u8; 20] = hex!("ae7ab96520de3a18e5e111b5eaab095312d7fe84");
pub const WSTETH_ADDRESS: [u8; 20] = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0");
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");

// stETH storage positions, each `keccak256` of the name Lido.sol v4.0.0 documents next to it.
// Lido packs two 128-bit scalars per slot; the low half is listed first.

/// `keccak256("lido.StETH.totalAndExternalShares")`: `totalShares` / `externalShares`.
pub const TOTAL_AND_EXTERNAL_SHARES_POSITION: [u8; 32] =
    hex!("6038150aecaa250d524370a0fdcdec13f2690e0723eaf277f41d7cae26b359e6");
/// `keccak256("lido.Lido.bufferedEtherAndDepositedPostReport")`: `bufferedEther` /
/// `depositedPostReport`, the ETH sent to the deposit contract since the last oracle report.
pub const BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_POSITION: [u8; 32] =
    hex!("81a11fa1111afa59b50051f60ccf604a39d96acb484dc467ad8eadb4a63f0a5f");
/// `keccak256("lido.Lido.clValidatorsBalanceAndClPendingBalance")`: `clValidatorsBalance` /
/// `clPendingBalance`, the consensus-layer balances as of the last oracle report.
pub const CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_POSITION: [u8; 32] =
    hex!("096e465397f38e659238ccd5d5a2c434ced54a63fd8d694045bfb058ab9d8112");
/// `keccak256("lido.Lido.stakeLimit")`
pub const STAKING_STATE_POSITION: [u8; 32] =
    hex!("a3678de4a579be090bed1177e0a24f77cc29d181ac22fd7688aca344d8938015");
/// `shares[wstETH]` in stETH's share mapping (mapping slot 0), i.e. `sharesOf(wstETH)`. The stETH
/// locked in the wrapper is all that unwrapping can pay out, so it bounds that direction.
pub const WSTETH_SHARES_POSITION: [u8; 32] =
    hex!("f37caed32e4e49c83636e0f1684f3f4a9a23c463a49eb17cd63abd50680b378b");

// One attribute per value the protocol names. A consumer reads `total_shares`, and the name
// holds across a slot relocation like the v3 -> v4 move.
pub const TOTAL_SHARES_ATTR: &str = "total_shares";
pub const EXTERNAL_SHARES_ATTR: &str = "external_shares";
pub const BUFFERED_ETHER_ATTR: &str = "buffered_ether";
pub const DEPOSITED_POST_REPORT_ATTR: &str = "deposited_post_report";
pub const CL_VALIDATORS_BALANCE_ATTR: &str = "cl_validators_balance";
pub const CL_PENDING_BALANCE_ATTR: &str = "cl_pending_balance";
pub const PREV_STAKE_BLOCK_NUMBER_ATTR: &str = "prev_stake_block_number";
pub const PREV_STAKE_LIMIT_ATTR: &str = "prev_stake_limit";
pub const MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR: &str = "max_stake_limit_growth_blocks";
pub const MAX_STAKE_LIMIT_ATTR: &str = "max_stake_limit";
pub const WSTETH_SHARES_ATTR: &str = "wsteth_shares";

/// Store keys holding the last seen raw value of each slot that feeds `totalPooledEther`, the
/// component's reported balance. A block that touches one of them usually leaves the others
/// untouched, so the latest value of each has to be carried across blocks. `sharesOf(wstETH)` is
/// not here: it bounds unwrapping but does not move the pool.
pub const TOTAL_AND_EXTERNAL_SHARES_KEY: &str = "total_and_external_shares";
pub const BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY: &str =
    "buffered_ether_and_deposited_post_report";
pub const CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY: &str =
    "cl_validators_balance_and_cl_pending_balance";

/// One value packed into a storage word, as `(value >> offset) & (2^width - 1)`.
pub struct PackedField {
    pub attribute: &'static str,
    pub offset: u32,
    pub width: u32,
}

/// A stETH storage slot this package tracks.
pub struct TrackedSlot {
    /// Raw storage position on the stETH contract.
    pub position: [u8; 32],
    /// The values packed into the word, reported one attribute each.
    pub fields: &'static [PackedField],
    /// Store key, for the slots that feed `totalPooledEther`. `None` for the slots that are
    /// reported as attributes but move no balance.
    pub balance_key: Option<&'static str>,
}

/// `totalShares` / `externalShares`.
pub const TOTAL_AND_EXTERNAL_SHARES_SLOT: TrackedSlot = TrackedSlot {
    position: TOTAL_AND_EXTERNAL_SHARES_POSITION,
    fields: &[
        PackedField { attribute: TOTAL_SHARES_ATTR, offset: 0, width: 128 },
        PackedField { attribute: EXTERNAL_SHARES_ATTR, offset: 128, width: 128 },
    ],
    balance_key: Some(TOTAL_AND_EXTERNAL_SHARES_KEY),
};

/// `bufferedEther` / `depositedPostReport`.
pub const BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT: TrackedSlot = TrackedSlot {
    position: BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_POSITION,
    fields: &[
        PackedField { attribute: BUFFERED_ETHER_ATTR, offset: 0, width: 128 },
        PackedField { attribute: DEPOSITED_POST_REPORT_ATTR, offset: 128, width: 128 },
    ],
    balance_key: Some(BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY),
};

/// `clValidatorsBalance` / `clPendingBalance`.
pub const CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT: TrackedSlot = TrackedSlot {
    position: CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_POSITION,
    fields: &[
        PackedField { attribute: CL_VALIDATORS_BALANCE_ATTR, offset: 0, width: 128 },
        PackedField { attribute: CL_PENDING_BALANCE_ATTR, offset: 128, width: 128 },
    ],
    balance_key: Some(CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY),
};

/// `StakeLimitUtils` packs four fields of two different widths into this one.
pub const STAKING_STATE_SLOT: TrackedSlot = TrackedSlot {
    position: STAKING_STATE_POSITION,
    fields: &[
        PackedField { attribute: PREV_STAKE_BLOCK_NUMBER_ATTR, offset: 0, width: 32 },
        PackedField { attribute: PREV_STAKE_LIMIT_ATTR, offset: 32, width: 96 },
        PackedField { attribute: MAX_STAKE_LIMIT_GROWTH_BLOCKS_ATTR, offset: 128, width: 32 },
        PackedField { attribute: MAX_STAKE_LIMIT_ATTR, offset: 160, width: 96 },
    ],
    balance_key: None,
};

/// `sharesOf(wstETH)`, a whole word.
pub const WSTETH_SHARES_SLOT: TrackedSlot = TrackedSlot {
    position: WSTETH_SHARES_POSITION,
    fields: &[PackedField { attribute: WSTETH_SHARES_ATTR, offset: 0, width: 256 }],
    balance_key: None,
};

/// Every tracked slot, so a storage write can be matched to its row by position.
pub const TRACKED_SLOTS: [TrackedSlot; 5] = [
    TOTAL_AND_EXTERNAL_SHARES_SLOT,
    BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT,
    CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT,
    STAKING_STATE_SLOT,
    WSTETH_SHARES_SLOT,
];

/// Lido's Aragon Kernel. stETH is an `AppProxyUpgradeable` that asks the Kernel for its
/// implementation on every call, so an upgrade is a write to the Kernel, not to stETH.
pub const LIDO_KERNEL_ADDRESS: [u8; 20] = hex!("b8ffc3cd6e7cf5a098a1c92f48009765b24088dc");
/// stETH's app id in the Kernel.
pub const STETH_APP_ID: [u8; 32] =
    hex!("3ca7c3e38968823ccb4c78ea688df41356f182ae1d159e4ee608d30d68cef320");
/// `keccak256("SetApp(bytes32,bytes32,address)")`: the Kernel emits it on every implementation
/// change, with the namespace and app id indexed and the new implementation in the data.
pub const ARAGON_SET_APP_TOPIC: [u8; 32] =
    hex!("2ec1ae0a449b7ae354b9dacfb3ade6b6332ba26b7fcbb935835fa39dd7263b23");
/// `keccak256("base")`: the Kernel namespace that holds app implementations.
pub const ARAGON_APP_BASES_NAMESPACE: [u8; 32] =
    hex!("f1f3eb40f5bc1ad1344716ced8b8a0431d840b5783aea1fd01786bc26f35ac0f");

/// A proxy whose storage this package reads, identified by its Kernel app id.
///
/// The tracked slots belong to the implementation recorded in the manifest's `implementations`
/// under `label`. Another implementation may lay its storage out differently, so the component
/// pauses on the block that installs one, until someone re-verifies the slots and records it.
pub struct TrackedProxy {
    pub label: &'static str,
    pub app_id: [u8; 32],
}

pub const STETH_PROXY: TrackedProxy = TrackedProxy { label: "steth", app_id: STETH_APP_ID };

/// Every proxy whose implementation change pauses the component. wstETH is not a proxy.
pub const TRACKED_PROXIES: [TrackedProxy; 1] = [STETH_PROXY];
