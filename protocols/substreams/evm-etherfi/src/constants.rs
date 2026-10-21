use substreams::hex;

/// Native ETH as Tycho addresses it (`Chain::native_token`), not the router's 0xEeee..EEeE
/// sentinel: the indexer prices tokens and the swap encoder matches ETH legs by this address.
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");
pub const EETH_ADDRESS: [u8; 20] = hex!("35fa164735182de50811e8e2e824cfb9b6118ac2");
pub const WEETH_ADDRESS: [u8; 20] = hex!("cd5fe23c85820f7b72d0926fc9b05b43e359b7ee");
pub const LIQUIDITY_POOL_ADDRESS: [u8; 20] = hex!("308861a430be4cce5502d0a12724771fc6daf216");
pub const REDEMPTION_MANAGER_ADDRESS: [u8; 20] = hex!("dadef1ffbfeaab4f68a9fd181395f68b4e4e7ae0");
/// `EtherFiRateLimiter`: eETH consumes one of its buckets on every mint and every burn.
pub const RATE_LIMITER_ADDRESS: [u8; 20] = hex!("6c7c54cfc2225fa985cd25f04d923b93c60a02f8");

/// The venue is two components. `Pool` is keyed by eETH and serves `LiquidityPool.deposit`
/// (ETH -> eETH) and `EtherFiRedemptionManager.redeemEEth` (eETH -> ETH). `Wrapper` is keyed by
/// weETH and serves `wrap` / `unwrap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component {
    Pool,
    Wrapper,
}

impl Component {
    pub fn id(self) -> &'static str {
        match self {
            Component::Pool => POOL_COMPONENT_ID,
            Component::Wrapper => WRAPPER_COMPONENT_ID,
        }
    }
}

pub const POOL_COMPONENT_ID: &str = "0x35fa164735182de50811e8e2e824cfb9b6118ac2";
pub const WRAPPER_COMPONENT_ID: &str = "0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee";

/// `keccak256("eip1967.proxy.implementation") - 1`: where each proxy this package reads keeps its
/// implementation, and the slot an upgrade writes.
pub const EIP1967_IMPLEMENTATION_POSITION: [u8; 32] =
    hex!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// A proxy whose storage or execution behavior this integration relies on.
///
/// The tracked slots belong to the implementation recorded in the manifest's `implementations`
/// under `label`. Another implementation may change its storage layout or swap behavior, so both
/// components pause on the block that installs one, until its storage and behavior are re-verified.
pub struct TrackedProxy {
    pub label: &'static str,
    pub proxy: [u8; 20],
}

/// Every proxy whose implementation change pauses the components. weETH upgrades can change
/// wrap and unwrap behavior even though its balance is read through eETH's share mapping.
pub const TRACKED_PROXIES: [TrackedProxy; 5] = [
    TrackedProxy { label: "liquidity_pool", proxy: LIQUIDITY_POOL_ADDRESS },
    TrackedProxy { label: "eeth", proxy: EETH_ADDRESS },
    TrackedProxy { label: "weeth", proxy: WEETH_ADDRESS },
    TrackedProxy { label: "redemption_manager", proxy: REDEMPTION_MANAGER_ADDRESS },
    TrackedProxy { label: "rate_limiter", proxy: RATE_LIMITER_ADDRESS },
];

// Storage positions, verified against the implementations the manifest records under
// `implementations`: LiquidityPool 0x17a16747d03006c9754548ac0d0aff48783a4a45, eETH
// 0xd1901dd36cbf4a81386d0162df2707f7ddb60527, EtherFiRedemptionManager
// 0x5d53b303d62a7861f88650045b8d5deb59dfb3dc, EtherFiRateLimiter
// 0x9ea4d0fd09b628e23b1998f2153e27e5261b1b67.

/// LiquidityPool slot 207: `totalValueOutOfLp` in the low half, `totalValueInLp` in the high
/// half, both `uint128`. Their sum is `getTotalPooledEther()`.
pub const LIQUIDITY_POOL_VALUE_POSITION: [u8; 32] =
    hex!("00000000000000000000000000000000000000000000000000000000000000cf");
/// eETH slot 202: `totalShares`.
pub const EETH_TOTAL_SHARES_POSITION: [u8; 32] =
    hex!("00000000000000000000000000000000000000000000000000000000000000ca");
/// `shares[weETH]` in eETH's share mapping (slot 203): the shares the wrapper holds, i.e.
/// `eETH.balanceOf(weETH)` before the share rate is applied. Unwrapping pays out of them.
pub const WEETH_SHARES_POSITION: [u8; 32] =
    hex!("65699867c563473027a12e5ab944a50581e6a89508c194c0a9c647f5f7b8d911");
/// `tokenToRedemptionInfo[0xEeee..EEeE]` (mapping slot 251), first word: the
/// `BucketLimiter.Limit` that rate-limits ETH redemptions.
pub const ETH_REDEMPTION_LIMIT_POSITION: [u8; 32] =
    hex!("de214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e4089");
/// Second word of the same struct: exit fee split, exit fee and low watermark, in basis points.
pub const ETH_REDEMPTION_INFO_POSITION: [u8; 32] =
    hex!("de214f9917f097ee519bb7c8046c126ea97c66e258d7d59038feae19259e408a");
/// `limits[keccak256("EETH_MINT_LIMIT_ID")]` on the rate limiter (mapping slot 201): the bucket
/// `eETH.mintShares` consumes, in gwei. Deposits are bounded by it.
pub const EETH_MINT_LIMIT_POSITION: [u8; 32] =
    hex!("307ba46f8ad0e50f846ef63910e4eaf48114447a0c6bed3a66bb4c9e19ac5c96");
/// `limits[keccak256("EETH_BURN_LIMIT_ID")]`: the bucket `eETH.burnShares` consumes, in gwei.
/// Redemptions burn shares and are bounded by it as well as by the redemption manager's own.
pub const EETH_BURN_LIMIT_POSITION: [u8; 32] =
    hex!("3f303c9df3b7d9b21f01cecce772249973e78530093cd404fcd79757440a074e");

// One attribute per value the protocol names, unpacked from the storage words above.
pub const TOTAL_VALUE_OUT_OF_LP_ATTR: &str = "total_value_out_of_lp";
pub const TOTAL_VALUE_IN_LP_ATTR: &str = "total_value_in_lp";
pub const TOTAL_SHARES_ATTR: &str = "total_shares";
pub const WEETH_SHARES_ATTR: &str = "weeth_shares";
pub const REDEMPTION_BUCKET_CAPACITY_ATTR: &str = "redemption_bucket_capacity";
pub const REDEMPTION_BUCKET_REMAINING_ATTR: &str = "redemption_bucket_remaining";
pub const REDEMPTION_BUCKET_LAST_REFILL_ATTR: &str = "redemption_bucket_last_refill";
pub const REDEMPTION_BUCKET_REFILL_RATE_ATTR: &str = "redemption_bucket_refill_rate";
pub const EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR: &str = "exit_fee_split_to_treasury_bps";
pub const EXIT_FEE_BPS_ATTR: &str = "exit_fee_bps";
pub const LOW_WATERMARK_BPS_ATTR: &str = "low_watermark_bps";
pub const MINT_BUCKET_CAPACITY_ATTR: &str = "mint_bucket_capacity";
pub const MINT_BUCKET_REMAINING_ATTR: &str = "mint_bucket_remaining";
pub const MINT_BUCKET_LAST_REFILL_ATTR: &str = "mint_bucket_last_refill";
pub const MINT_BUCKET_REFILL_RATE_ATTR: &str = "mint_bucket_refill_rate";
pub const BURN_BUCKET_CAPACITY_ATTR: &str = "burn_bucket_capacity";
pub const BURN_BUCKET_REMAINING_ATTR: &str = "burn_bucket_remaining";
pub const BURN_BUCKET_LAST_REFILL_ATTR: &str = "burn_bucket_last_refill";
pub const BURN_BUCKET_REFILL_RATE_ATTR: &str = "burn_bucket_refill_rate";

/// Store keys for the slots the component balances are derived from. A block that moves one of
/// them usually leaves the others alone, so the latest value of each is carried across blocks.
pub const LIQUIDITY_POOL_VALUE_KEY: &str = "liquidity_pool_value";
pub const TOTAL_SHARES_KEY: &str = "total_shares";
pub const WEETH_SHARES_KEY: &str = "weeth_shares";

/// One value packed into a storage word, as `(value >> offset) & (2^width - 1)`.
pub struct PackedField {
    pub attribute: &'static str,
    pub offset: u32,
    pub width: u32,
}

/// A storage slot this package tracks, on a specific contract.
pub struct TrackedSlot {
    pub contract: [u8; 20],
    pub position: [u8; 32],
    /// The values packed into the word, reported one attribute each.
    pub fields: &'static [PackedField],
    /// The components that carry those attributes.
    pub components: &'static [Component],
    /// Store key, for the slots a component balance is derived from.
    pub balance_key: Option<&'static str>,
}

/// `BucketLimiter.Limit`: four `uint64`s packed low to high.
const fn bucket_fields(
    capacity: &'static str,
    remaining: &'static str,
    last_refill: &'static str,
    refill_rate: &'static str,
) -> [PackedField; 4] {
    [
        PackedField { attribute: capacity, offset: 0, width: 64 },
        PackedField { attribute: remaining, offset: 64, width: 64 },
        PackedField { attribute: last_refill, offset: 128, width: 64 },
        PackedField { attribute: refill_rate, offset: 192, width: 64 },
    ]
}

pub const LIQUIDITY_POOL_VALUE_SLOT: TrackedSlot = TrackedSlot {
    contract: LIQUIDITY_POOL_ADDRESS,
    position: LIQUIDITY_POOL_VALUE_POSITION,
    fields: &[
        PackedField { attribute: TOTAL_VALUE_OUT_OF_LP_ATTR, offset: 0, width: 128 },
        PackedField { attribute: TOTAL_VALUE_IN_LP_ATTR, offset: 128, width: 128 },
    ],
    components: &[Component::Pool, Component::Wrapper],
    balance_key: Some(LIQUIDITY_POOL_VALUE_KEY),
};

pub const EETH_TOTAL_SHARES_SLOT: TrackedSlot = TrackedSlot {
    contract: EETH_ADDRESS,
    position: EETH_TOTAL_SHARES_POSITION,
    fields: &[PackedField { attribute: TOTAL_SHARES_ATTR, offset: 0, width: 256 }],
    components: &[Component::Pool, Component::Wrapper],
    balance_key: Some(TOTAL_SHARES_KEY),
};

pub const WEETH_SHARES_SLOT: TrackedSlot = TrackedSlot {
    contract: EETH_ADDRESS,
    position: WEETH_SHARES_POSITION,
    fields: &[PackedField { attribute: WEETH_SHARES_ATTR, offset: 0, width: 256 }],
    components: &[Component::Wrapper],
    balance_key: Some(WEETH_SHARES_KEY),
};

pub const ETH_REDEMPTION_LIMIT_SLOT: TrackedSlot = TrackedSlot {
    contract: REDEMPTION_MANAGER_ADDRESS,
    position: ETH_REDEMPTION_LIMIT_POSITION,
    fields: &bucket_fields(
        REDEMPTION_BUCKET_CAPACITY_ATTR,
        REDEMPTION_BUCKET_REMAINING_ATTR,
        REDEMPTION_BUCKET_LAST_REFILL_ATTR,
        REDEMPTION_BUCKET_REFILL_RATE_ATTR,
    ),
    components: &[Component::Pool],
    balance_key: None,
};

pub const ETH_REDEMPTION_INFO_SLOT: TrackedSlot = TrackedSlot {
    contract: REDEMPTION_MANAGER_ADDRESS,
    position: ETH_REDEMPTION_INFO_POSITION,
    fields: &[
        PackedField { attribute: EXIT_FEE_SPLIT_TO_TREASURY_BPS_ATTR, offset: 0, width: 16 },
        PackedField { attribute: EXIT_FEE_BPS_ATTR, offset: 16, width: 16 },
        PackedField { attribute: LOW_WATERMARK_BPS_ATTR, offset: 32, width: 16 },
    ],
    components: &[Component::Pool],
    balance_key: None,
};

pub const EETH_MINT_LIMIT_SLOT: TrackedSlot = TrackedSlot {
    contract: RATE_LIMITER_ADDRESS,
    position: EETH_MINT_LIMIT_POSITION,
    fields: &bucket_fields(
        MINT_BUCKET_CAPACITY_ATTR,
        MINT_BUCKET_REMAINING_ATTR,
        MINT_BUCKET_LAST_REFILL_ATTR,
        MINT_BUCKET_REFILL_RATE_ATTR,
    ),
    components: &[Component::Pool],
    balance_key: None,
};

pub const EETH_BURN_LIMIT_SLOT: TrackedSlot = TrackedSlot {
    contract: RATE_LIMITER_ADDRESS,
    position: EETH_BURN_LIMIT_POSITION,
    fields: &bucket_fields(
        BURN_BUCKET_CAPACITY_ATTR,
        BURN_BUCKET_REMAINING_ATTR,
        BURN_BUCKET_LAST_REFILL_ATTR,
        BURN_BUCKET_REFILL_RATE_ATTR,
    ),
    components: &[Component::Pool],
    balance_key: None,
};

/// Every tracked slot, so a storage write can be matched to its row by contract and position.
pub const TRACKED_SLOTS: [TrackedSlot; 7] = [
    LIQUIDITY_POOL_VALUE_SLOT,
    EETH_TOTAL_SHARES_SLOT,
    WEETH_SHARES_SLOT,
    ETH_REDEMPTION_LIMIT_SLOT,
    ETH_REDEMPTION_INFO_SLOT,
    EETH_MINT_LIMIT_SLOT,
    EETH_BURN_LIMIT_SLOT,
];
