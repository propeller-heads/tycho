//! Names the cause of a failed Aqua0 fill from the transaction's revert data.

use alloy::{
    primitives::{Bytes, FixedBytes},
    sol,
    sol_types::{sol_data, SolError, SolInterface, SolType},
};

sol! {
    /// Uniswap V4 wraps a hook's revert in this.
    #[derive(Debug, PartialEq, Eq)]
    error WrappedError(address target, bytes4 selector, bytes reason, bytes details);

    /// Every error a fill through Aqua0's hook can end with: the hook's own and those of the asset
    /// vaults it settles through in `afterSwap`.
    #[derive(Debug, PartialEq, Eq)]
    interface Aqua0Hook {
        error EnforcedPause();
        error UnsignedSwap();
        error InvalidSignature();
        error JitPositionPaused();
        error JitSwapMismatch();
        error JitExecutorMismatch(address sender, address executor);
        error ClassNotPool(bytes32 poolId, uint256 classId);
        error ExpiredDeadline(uint256 deadline, uint256 timestamp);
        error ECDSAInvalidSignature();
        error ECDSAInvalidSignatureLength(uint256 length);
        error ECDSAInvalidSignatureS(bytes32 s);
        error NonceUsed(uint256 nonce);
        error NoRanges();
        error TooManyRanges(uint256 provided, uint256 max);
        error DrawsNotSorted(address lp, uint8 position);
        error TokenHasNoVault(address token);
        error RangeNotCommitted(uint256 strategyId, address lp, uint8 position);
        error RangeNotSingleSided(address lp, uint8 position);
        error PositionNotOnOutputSide(address lp, uint8 position);
        error PositionOverdrawn(address lp, uint8 position);
        error FillOutsideItsPositions(uint160 sqrtPriceX96);
        error RanOutOfBacking(uint256 strategyId, uint256 shortfall);
        error InsufficientSharedIdle(uint256 requested, uint256 available);
        error TooManyDraws(uint256 count, uint256 max);
        error ClassNotRegistered(uint256 classId);
        error ClassIsPaused(uint256 classId);
        error StrategyVenueBound(uint256 strategyId, address venue);
        error AccessControlUnauthorizedAccount(address account, bytes32 role);
        error ReentrancyGuardReentrantCall();
        error NotPoolManager();
        error ZeroAmount();
        error ZeroAddress();
        error SafeERC20FailedOperation(address token);
    }
}

pub use Aqua0Hook::Aqua0HookErrors;

/// `Error(string)`. The V4 executor puts the pool manager's raw revert bytes in its string.
const ERROR_STRING: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
/// The hook's `beforeSwap` selector, as `WrappedError` names it.
pub const BEFORE_SWAP: [u8; 4] = [0x57, 0x5e, 0x24, 0xb4];
/// The hook's `afterSwap` selector, as `WrappedError` names it.
pub const AFTER_SWAP: [u8; 4] = [0xb4, 0x7b, 0x2f, 0xb1];

/// The most layers of `Error(string)` and `WrappedError` looked through.
const MAX_LAYERS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aqua0Revert {
    /// The hook or a vault it settles through refused the fill. `callback` is `BEFORE_SWAP` or
    /// `AFTER_SWAP`, or zero when the data was the bare error.
    Hook { callback: FixedBytes<4>, error: Aqua0HookErrors },
    /// Anything else, unwrapped as far as it goes.
    Other { data: Bytes },
}

/// Looks through `Error(string)` and `WrappedError`, in any order and up to four layers, and names
/// the Aqua0 error inside.
pub fn decode_revert(data: &[u8]) -> Aqua0Revert {
    let mut current = Bytes::copy_from_slice(data);
    let mut callback = FixedBytes::ZERO;
    for _ in 0..MAX_LAYERS {
        if current.starts_with(&ERROR_STRING) {
            // The string holds raw bytes, so read it as `bytes`, which is encoded the same way.
            let Ok((inner,)) = <(sol_data::Bytes,)>::abi_decode_params(&current[4..]) else {
                break;
            };
            current = inner;
        } else if let Ok(wrapped) = WrappedError::abi_decode(&current) {
            callback = wrapped.selector;
            current = wrapped.reason;
        } else {
            break;
        }
    }
    match Aqua0HookErrors::abi_decode(&current) {
        Ok(error) => Aqua0Revert::Hook { callback, error },
        Err(_) => Aqua0Revert::Other { data: current },
    }
}

impl Aqua0Revert {
    /// A plain cause a solver can act on.
    pub fn cause(&self) -> &'static str {
        use Aqua0HookErrors as E;
        match self {
            Self::Hook { error, .. } => match error {
                E::ExpiredDeadline(_) => "quote expired",
                E::NonceUsed(_) => "quote already used",
                E::JitExecutorMismatch(_) => "swap sent through a router the quote does not name",
                E::JitSwapMismatch(_) => {
                    "swap differs from the quote (input amount, direction or price limit)"
                }
                E::PositionNotOnOutputSide(_) | E::RangeNotSingleSided(_) => {
                    "pool moved since the quote: an earlier fill on this market landed first"
                }
                E::PositionOverdrawn(_) => "backing used, or the pool moved, since the quote",
                E::RanOutOfBacking(_) | E::InsufficientSharedIdle(_) => {
                    "backing used since the quote"
                }
                E::RangeNotCommitted(_) => "LP changed its range since the quote",
                E::EnforcedPause(_) | E::JitPositionPaused(_) | E::ClassIsPaused(_) => "paused",
                E::UnsignedSwap(_) => "no Aqua0 quote in the hook data",
                E::InvalidSignature(_) |
                E::ECDSAInvalidSignature(_) |
                E::ECDSAInvalidSignatureLength(_) |
                E::ECDSAInvalidSignatureS(_) |
                E::ClassNotPool(_) |
                E::NoRanges(_) |
                E::TooManyRanges(_) |
                E::DrawsNotSorted(_) => "quote is malformed or not signed by Aqua0",
                E::TokenHasNoVault(_) |
                E::TooManyDraws(_) |
                E::ClassNotRegistered(_) |
                E::StrategyVenueBound(_) |
                E::AccessControlUnauthorizedAccount(_) => "Aqua0 market misconfigured",
                E::FillOutsideItsPositions(_) |
                E::ReentrancyGuardReentrantCall(_) |
                E::NotPoolManager(_) |
                E::ZeroAmount(_) |
                E::ZeroAddress(_) |
                E::SafeERC20FailedOperation(_) => "fill refused by the hook",
            },
            Self::Other { .. } => "not an Aqua0 hook error",
        }
    }

    /// The error's name, for example `PositionNotOnOutputSide`, or `None` when it is not one of
    /// Aqua0's.
    pub fn name(&self) -> Option<&'static str> {
        match self {
            Self::Hook { error, .. } => Some(error_name(error)),
            Self::Other { .. } => None,
        }
    }
}

fn error_name(error: &Aqua0HookErrors) -> &'static str {
    let selector = error.selector();
    Aqua0HookErrors::SELECTORS
        .iter()
        .position(|known| *known == selector)
        .and_then(|index| {
            Aqua0HookErrors::SIGNATURES
                .get(index)
                .copied()
        })
        .and_then(|signature| signature.split('(').next())
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use alloy::{
        hex,
        primitives::{address, b256, Address, U160, U256},
    };
    use rstest::rstest;

    use super::{Aqua0Hook as H, *};

    const HOOK: Address = address!("90a99a9c0B2Fe748e6600dc4A9A8b814292428C0");
    const LP: Address = address!("9b080ef51af94d120ce7b23b9a375bc7b0f8a044");

    /// Zach's revert from the hook's beforeSwap, Arbitrum fork at block 527056494 (PR #1372
    /// review).
    const ZACH_INNER: [u8; 68] = hex!(
        "fbeb6447"
        "0000000000000000000000009b080ef51af94d120ce7b23b9a375bc7b0f8a044"
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    /// Zach's error inside `WrappedError` inside `Error(string)`, rebuilt from v4-core's
    /// `CustomRevert` and Tycho's `UniswapV4Executor` and checked with `cast`.
    const ZACH_WRAPPED: &str = "0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000014490bfb86500000000000000000000000090a99a9c0b2fe748e6600dc4a9a8b814292428c0575e24b400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000044fbeb64470000000000000000000000009b080ef51af94d120ce7b23b9a375bc7b0f8a0440000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004a9e35b2f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    /// Router revert data captured through Tycho Router V3, the V4 executor and Aqua0's live hook
    /// on an Arbitrum fork at block 527174560, from binding quotes Aqua0 signed for that router
    /// (never sent on chain): a second same-direction quote after the first filled.
    const ROUTER_RANGE_NOT_SINGLE_SIDED: &[u8] = &hex!(
        "08c379a0"
        "0000000000000000000000000000000000000000000000000000000000000020"
        "0000000000000000000000000000000000000000000000000000000000000144"
        "90bfb86500000000000000000000000090a99a9c0b2fe748e6600dc4a9a8b814"
        "292428c0575e24b4000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "0000008000000000000000000000000000000000000000000000000000000000"
        "0000010000000000000000000000000000000000000000000000000000000000"
        "000000441a0fc9ca0000000000000000000000009b080ef51af94d120ce7b23b"
        "9a375bc7b0f8a044000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "00000004a9e35b2f000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    /// As above: an opposite-direction quote after the first filled.
    const ROUTER_POSITION_OVERDRAWN: &[u8] = &hex!(
        "08c379a0"
        "0000000000000000000000000000000000000000000000000000000000000020"
        "0000000000000000000000000000000000000000000000000000000000000144"
        "90bfb86500000000000000000000000090a99a9c0b2fe748e6600dc4a9a8b814"
        "292428c0575e24b4000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "0000008000000000000000000000000000000000000000000000000000000000"
        "0000010000000000000000000000000000000000000000000000000000000000"
        "000000449da847190000000000000000000000009b080ef51af94d120ce7b23b"
        "9a375bc7b0f8a044000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "00000004a9e35b2f000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    /// As above: the first quote's calldata sent a second time.
    const ROUTER_NONCE_USED: &[u8] = &hex!(
        "08c379a0"
        "0000000000000000000000000000000000000000000000000000000000000020"
        "0000000000000000000000000000000000000000000000000000000000000124"
        "90bfb86500000000000000000000000090a99a9c0b2fe748e6600dc4a9a8b814"
        "292428c0575e24b4000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "0000008000000000000000000000000000000000000000000000000000000000"
        "000000e000000000000000000000000000000000000000000000000000000000"
        "000000241be5cb2ffce4fd4e4aae79729e35ac069c00b8c5b0f8dc9a7a286b42"
        "6ff59708c49ea855000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "00000004a9e35b2f000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    /// Captured the same way on a fork of a later block, from a test run whose hook data was a
    /// placeholder rather than an Aqua0 quote.
    const ROUTER_UNSIGNED_SWAP: &[u8] = &hex!(
        "08c379a0"
        "0000000000000000000000000000000000000000000000000000000000000020"
        "0000000000000000000000000000000000000000000000000000000000000104"
        "90bfb86500000000000000000000000090a99a9c0b2fe748e6600dc4a9a8b814"
        "292428c0575e24b4000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "0000008000000000000000000000000000000000000000000000000000000000"
        "000000c000000000000000000000000000000000000000000000000000000000"
        "00000004fbdd5f9b000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
        "00000004a9e35b2f000000000000000000000000000000000000000000000000"
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    fn expected() -> Aqua0HookErrors {
        Aqua0HookErrors::PositionNotOnOutputSide(H::PositionNotOnOutputSide { lp: LP, position: 0 })
    }

    /// `inner` the way the router returns it: in the pool manager's `WrappedError`, inside the
    /// executor's `Error(string)`.
    fn wrap(callback: [u8; 4], inner: &[u8]) -> Vec<u8> {
        let wrapped = WrappedError {
            target: HOOK,
            selector: callback.into(),
            reason: inner.to_vec().into(),
            details: hex!("a9e35b2f").to_vec().into(),
        }
        .abi_encode();
        let mut outer = ERROR_STRING.to_vec();
        outer.extend(<(sol_data::Bytes,)>::abi_encode_params(&(Bytes::from(wrapped),)));
        outer
    }

    #[test]
    fn names_the_bare_hook_error() {
        let decoded = decode_revert(&ZACH_INNER);
        assert_eq!(decoded, Aqua0Revert::Hook { callback: FixedBytes::ZERO, error: expected() });
        assert_eq!(decoded.name(), Some("PositionNotOnOutputSide"));
    }

    #[test]
    fn looks_through_error_string_and_wrapped_error() {
        let decoded = decode_revert(&wrap(BEFORE_SWAP, &ZACH_INNER));
        assert_eq!(decoded, Aqua0Revert::Hook { callback: BEFORE_SWAP.into(), error: expected() });
        assert!(decoded.cause().contains("pool moved"));
    }

    #[test]
    fn decodes_the_full_router_revert() {
        let data = hex::decode(ZACH_WRAPPED).unwrap();
        assert_eq!(data, wrap(BEFORE_SWAP, &ZACH_INNER));
        assert_eq!(
            decode_revert(&data),
            Aqua0Revert::Hook { callback: BEFORE_SWAP.into(), error: expected() }
        );
    }

    #[test]
    fn the_wrapped_error_selector_is_uniswaps() {
        assert_eq!(WrappedError::SELECTOR, hex!("90bfb865"));
    }

    /// One value per error, with the selector the README lists for it.
    fn every_error() -> Vec<(&'static str, [u8; 4], Aqua0HookErrors)> {
        use Aqua0HookErrors as E;
        let n = U256::from(7u64);
        let b = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let other = address!("1b82e8c16ce8e17535c9b6b8ad107f61b0b192b5");
        vec![
            ("EnforcedPause", hex!("d93c0665"), E::EnforcedPause(H::EnforcedPause {})),
            ("UnsignedSwap", hex!("fbdd5f9b"), E::UnsignedSwap(H::UnsignedSwap {})),
            ("InvalidSignature", hex!("8baa579f"), E::InvalidSignature(H::InvalidSignature {})),
            ("JitPositionPaused", hex!("3c853ab7"), E::JitPositionPaused(H::JitPositionPaused {})),
            ("JitSwapMismatch", hex!("2ecbaf3f"), E::JitSwapMismatch(H::JitSwapMismatch {})),
            (
                "JitExecutorMismatch",
                hex!("a2f78b35"),
                E::JitExecutorMismatch(H::JitExecutorMismatch { sender: other, executor: HOOK }),
            ),
            (
                "ClassNotPool",
                hex!("a1380257"),
                E::ClassNotPool(H::ClassNotPool { poolId: b, classId: n }),
            ),
            (
                "ExpiredDeadline",
                hex!("c1f738dc"),
                E::ExpiredDeadline(H::ExpiredDeadline { deadline: n, timestamp: n + U256::ONE }),
            ),
            (
                "ECDSAInvalidSignature",
                hex!("f645eedf"),
                E::ECDSAInvalidSignature(H::ECDSAInvalidSignature {}),
            ),
            (
                "ECDSAInvalidSignatureLength",
                hex!("fce698f7"),
                E::ECDSAInvalidSignatureLength(H::ECDSAInvalidSignatureLength { length: n }),
            ),
            (
                "ECDSAInvalidSignatureS",
                hex!("d78bce0c"),
                E::ECDSAInvalidSignatureS(H::ECDSAInvalidSignatureS { s: b }),
            ),
            ("NonceUsed", hex!("1be5cb2f"), E::NonceUsed(H::NonceUsed { nonce: n })),
            ("NoRanges", hex!("6a05e465"), E::NoRanges(H::NoRanges {})),
            (
                "TooManyRanges",
                hex!("34f94fea"),
                E::TooManyRanges(H::TooManyRanges { provided: n, max: U256::from(4u64) }),
            ),
            (
                "DrawsNotSorted",
                hex!("a2e9ebdb"),
                E::DrawsNotSorted(H::DrawsNotSorted { lp: LP, position: 1 }),
            ),
            (
                "TokenHasNoVault",
                hex!("0a057e82"),
                E::TokenHasNoVault(H::TokenHasNoVault { token: other }),
            ),
            (
                "RangeNotCommitted",
                hex!("8f9dff3c"),
                E::RangeNotCommitted(H::RangeNotCommitted { strategyId: n, lp: LP, position: 2 }),
            ),
            (
                "RangeNotSingleSided",
                hex!("1a0fc9ca"),
                E::RangeNotSingleSided(H::RangeNotSingleSided { lp: LP, position: 0 }),
            ),
            ("PositionNotOnOutputSide", hex!("fbeb6447"), expected()),
            (
                "PositionOverdrawn",
                hex!("9da84719"),
                E::PositionOverdrawn(H::PositionOverdrawn { lp: LP, position: 3 }),
            ),
            (
                "FillOutsideItsPositions",
                hex!("8bf35716"),
                E::FillOutsideItsPositions(H::FillOutsideItsPositions {
                    sqrtPriceX96: U160::from(1u64) << 96,
                }),
            ),
            (
                "RanOutOfBacking",
                hex!("dc7c66a9"),
                E::RanOutOfBacking(H::RanOutOfBacking { strategyId: n, shortfall: n }),
            ),
            (
                "InsufficientSharedIdle",
                hex!("0135bcdc"),
                E::InsufficientSharedIdle(H::InsufficientSharedIdle {
                    requested: n,
                    available: U256::ZERO,
                }),
            ),
            (
                "TooManyDraws",
                hex!("92fde8f4"),
                E::TooManyDraws(H::TooManyDraws { count: n, max: U256::from(4u64) }),
            ),
            (
                "ClassNotRegistered",
                hex!("f1f99109"),
                E::ClassNotRegistered(H::ClassNotRegistered { classId: n }),
            ),
            ("ClassIsPaused", hex!("919a8006"), E::ClassIsPaused(H::ClassIsPaused { classId: n })),
            (
                "StrategyVenueBound",
                hex!("7dd3ea88"),
                E::StrategyVenueBound(H::StrategyVenueBound { strategyId: n, venue: other }),
            ),
            (
                "AccessControlUnauthorizedAccount",
                hex!("e2517d3f"),
                E::AccessControlUnauthorizedAccount(H::AccessControlUnauthorizedAccount {
                    account: HOOK,
                    role: b,
                }),
            ),
            (
                "ReentrancyGuardReentrantCall",
                hex!("3ee5aeb5"),
                E::ReentrancyGuardReentrantCall(H::ReentrancyGuardReentrantCall {}),
            ),
            ("NotPoolManager", hex!("ae18210a"), E::NotPoolManager(H::NotPoolManager {})),
            ("ZeroAmount", hex!("1f2a2005"), E::ZeroAmount(H::ZeroAmount {})),
            ("ZeroAddress", hex!("d92e233d"), E::ZeroAddress(H::ZeroAddress {})),
            (
                "SafeERC20FailedOperation",
                hex!("5274afe7"),
                E::SafeERC20FailedOperation(H::SafeERC20FailedOperation { token: other }),
            ),
        ]
    }

    #[test]
    fn every_error_round_trips() {
        let errors = every_error();
        assert_eq!(errors.len(), Aqua0HookErrors::SELECTORS.len(), "one test value per error");
        for (name, selector, error) in errors {
            assert_eq!(error.selector(), selector, "{name} selector");
            let inner = error.abi_encode();
            for callback in [BEFORE_SWAP, AFTER_SWAP] {
                let decoded = decode_revert(&wrap(callback, &inner));
                assert_eq!(decoded.name(), Some(name));
                assert_eq!(
                    decoded,
                    Aqua0Revert::Hook { callback: callback.into(), error: error.clone() },
                    "{name}"
                );
            }
            assert_eq!(
                decode_revert(&inner),
                Aqua0Revert::Hook { callback: FixedBytes::ZERO, error },
                "{name} bare"
            );
        }
    }

    #[rstest]
    #[case::unknown_selector(hex!("deadbeef").to_vec())]
    #[case::empty(Vec::new())]
    #[case::panic(hex!("4e487b710000000000000000000000000000000000000000000000000000000000000011").to_vec())]
    #[case::truncated_error_string(hex!("08c379a000").to_vec())]
    fn anything_else_comes_back_as_raw_bytes(#[case] data: Vec<u8>) {
        let decoded = decode_revert(&data);
        assert_eq!(decoded, Aqua0Revert::Other { data: data.into() });
        assert_eq!(decoded.name(), None);
        assert_eq!(decoded.cause(), "not an Aqua0 hook error");
    }

    #[test]
    fn an_unknown_error_inside_the_wrappers_is_unwrapped() {
        let inner = hex!("deadbeef");
        assert_eq!(
            decode_revert(&wrap(AFTER_SWAP, &inner)),
            Aqua0Revert::Other { data: inner.to_vec().into() }
        );
    }

    /// Real router reverts decode to the hook error inside, from `beforeSwap`.
    #[rstest]
    #[case::same_direction_after_a_fill(
        ROUTER_RANGE_NOT_SINGLE_SIDED,
        Aqua0HookErrors::RangeNotSingleSided(H::RangeNotSingleSided { lp: LP, position: 0 })
    )]
    #[case::opposite_direction_after_a_fill(
        ROUTER_POSITION_OVERDRAWN,
        Aqua0HookErrors::PositionOverdrawn(H::PositionOverdrawn { lp: LP, position: 0 })
    )]
    #[case::sent_twice(
        ROUTER_NONCE_USED,
        Aqua0HookErrors::NonceUsed(H::NonceUsed {
            nonce: U256::from_be_slice(&hex!(
                "fce4fd4e4aae79729e35ac069c00b8c5b0f8dc9a7a286b426ff59708c49ea855"
            )),
        })
    )]
    #[case::no_quote(ROUTER_UNSIGNED_SWAP, Aqua0HookErrors::UnsignedSwap(H::UnsignedSwap {}))]
    fn decodes_captured_router_reverts(#[case] data: &[u8], #[case] error: Aqua0HookErrors) {
        assert_eq!(decode_revert(data), Aqua0Revert::Hook { callback: BEFORE_SWAP.into(), error });
    }

    /// The README's errors table lists every error the decoder knows, each with its selector,
    /// and nothing else.
    #[test]
    fn the_readme_lists_every_error_with_its_selector() {
        let readme = include_str!("README.md");
        let rows: Vec<&str> = readme
            .lines()
            .filter(|line| line.starts_with("| `"))
            .collect();
        for (signature, selector) in Aqua0HookErrors::SIGNATURES
            .iter()
            .zip(Aqua0HookErrors::SELECTORS)
        {
            let row = format!("| `{signature}` | `0x{}` |", hex::encode(selector));
            assert!(
                rows.iter()
                    .any(|line| line.starts_with(&row)),
                "README misses {row}"
            );
        }
        assert_eq!(rows.len(), Aqua0HookErrors::SIGNATURES.len(), "README lists other errors");
    }
}
