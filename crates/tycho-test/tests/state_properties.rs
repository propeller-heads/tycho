//! Property tests of `ProtocolSim` states: every state the `state_properties!` call names is
//! checked against `tycho_test::checks::properties`, on each of its corner cases at every corner
//! of `AnyAmount`, and on generated cases at generated amounts.
//!
//! Both runs are deterministic: the corners are fixed, and the generated run uses a fixed seed and
//! 256 cases. Proptest shrinks a generated failure to a minimal case and saves it under
//! `proptest-regressions/`. Commit that file, and the case runs first from then on.

use itertools::{iproduct, Itertools};
use num_bigint::BigUint;
use tycho_simulation::evm::protocol::{
    checkable::generator::{
        proptest::{
            prelude::*,
            test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner},
        },
        AnyUint, Case, Domain, DomainExt, SetOf, StateCases,
    },
    uniswap_v2::state::UniswapV2State,
};
use tycho_test::checks::{amounts::Amount, properties::check_state};

/// A test module per state, named after it in snake case, with a test of its corner states and
/// one of its generated states.
macro_rules! state_properties {
    ($($state:ident),+ $(,)?) => {
        $(pastey::paste! {
            mod [<$state:snake>] {
                use super::*;

                #[test]
                fn corners() {
                    check_corner_cases::<$state>();
                }

                #[test]
                fn random() {
                    check_random_cases::<$state>();
                }
            }
        })+
    };
}

state_properties!(UniswapV2State);

/// Checks every corner case at every corner of [`AnyAmount`], and reports all that fail.
fn check_corner_cases<S: StateCases>() {
    let corners = iproduct!(S::cases().corners(), SetOf(AnyAmount).corners()).collect_vec();
    let failures = corners
        .iter()
        .filter_map(|(case, amounts)| failure(case, amounts))
        .collect_vec();
    assert!(
        failures.is_empty(),
        "{} of {} corner cases fail:\n\n{}",
        failures.len(),
        corners.len(),
        failures.join("\n\n")
    );
}

/// The seed of every run, so CI and a local run test the same cases.
const SEED: u64 = 0x7963_686f;

fn check_random_cases<S: StateCases>() {
    let config = Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(SEED),
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/proptest-regressions/state_properties.txt"
        )))),
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    let cases = S::cases();
    let strategy = (cases.random(), SetOf(AnyAmount).corner_or_random());
    runner
        .run(&strategy, |(case, amounts)| {
            failure(&case, &amounts).map_or(Ok(()), |failure| Err(TestCaseError::fail(failure)))
        })
        .unwrap_or_else(|e| panic!("{e}"));
}

/// What is wrong with a case quoted at `amounts`, if anything: that its state is invalid, or
/// the properties it breaks.
fn failure<S: StateCases>(case: &Case<S, S::Tokens>, amounts: &[Amount]) -> Option<String> {
    let tokens = case.tokens.as_ref();
    let state = &case.state;
    let described = format!(
        "{state:?} with tokens {:?}",
        tokens
            .iter()
            .map(|token| (&token.address, token.decimals))
            .collect_vec()
    );
    if let Err(e) = state.check_valid() {
        return Some(format!("{described} is invalid: {e}"));
    }
    let violations = check_state(state, tokens, amounts);
    (!violations.is_empty()).then(|| format!("{described}:\n{}", violations.iter().join("\n")))
}

/// An amount to quote a direction at; a case quotes at a [`SetOf`] of them. The corners are the
/// boundaries around zero, the limit and the `U256` edge. Random amounts are fractions of up to
/// three times the limit, raw amounts of up to 300 bits, past `U256`, where dust rounds to zero
/// output, and amounts near the limit.
#[derive(Debug, Clone, Copy)]
struct AnyAmount;

impl Domain for AnyAmount {
    type Value = Amount;

    fn corners(&self) -> Vec<Amount> {
        vec![
            Amount::Raw(BigUint::ZERO),
            Amount::Raw(BigUint::from(1u8)),
            // The limit and one unit around it.
            Amount::LimitPlus(-1),
            Amount::LimitPlus(0),
            Amount::LimitPlus(1),
            // A millionth of the limit, the limit less and plus a millionth, and twice it.
            Amount::PartsPerMillion(1),
            Amount::PartsPerMillion(999_999),
            Amount::PartsPerMillion(1_000_001),
            Amount::PartsPerMillion(2_000_000),
            // The largest amount a `U256` holds, and the smallest it does not.
            Amount::Raw((BigUint::from(1u8) << 256) - 1u8),
            Amount::Raw(BigUint::from(1u8) << 256),
        ]
    }

    fn random(&self) -> impl Strategy<Value = Amount> {
        let parts_per_million = (0u64..=3_000_000).prop_map(Amount::PartsPerMillion);
        let raw = AnyUint::<300, 5>
            .random()
            .prop_map(|raw| Amount::Raw(raw.into()));
        let near_limit = (-1_000i64..=1_000).prop_map(Amount::LimitPlus);
        prop_oneof![
            4 => parts_per_million,
            2 => raw,
            1 => near_limit,
        ]
    }
}
