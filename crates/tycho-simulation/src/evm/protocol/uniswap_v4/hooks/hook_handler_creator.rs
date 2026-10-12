use std::{
    collections::HashMap,
    str::FromStr,
    sync::{LazyLock, RwLock},
};

use alloy::primitives::{Address, U256};
use revm::{primitives::KECCAK_EMPTY, state::AccountInfo};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::errors::SimulationError,
    Bytes,
};

use crate::{
    evm::{
        engine_db::{create_engine, engine_db_interface::EngineDatabaseInterface, SHARED_TYCHO_DB},
        protocol::{
            uniswap_v4::{
                hooks::{
                    angstrom::hook_handler_creator::AngstromHookCreator,
                    generic_vm_hook_handler::GenericVMHookHandler,
                    hook_handler::HookHandler,
                    pons_v2::{
                        hook_handler::PONS_V2_HOOK_ROBINHOOD,
                        hook_handler_creator::PonsV2HookCreator,
                    },
                },
                state::UniswapV4State,
            },
            vm::constants::EXTERNAL_ACCOUNT,
        },
    },
    protocol::errors::InvalidSnapshotError,
};

/// Parameters for creating a HookHandler.
pub struct HookCreationParams<'a> {
    hook_address: Address,
    account_balances: &'a HashMap<Bytes, HashMap<Bytes, Bytes>>,
    all_tokens: &'a HashMap<Bytes, Token>,
    #[allow(dead_code)]
    state: UniswapV4State,
    /// Attributes of the component. If an attribute's value is a `bigint`,
    /// it will be encoded as a big endian signed hex string. See ResponseProtocolState for more
    /// details.
    pub(crate) attributes: &'a HashMap<String, Bytes>,
    #[allow(dead_code)]
    /// Mapping from token address to big-endian encoded balance for this component.
    balances: &'a HashMap<Bytes, Bytes>,
    /// Show vm traces in simulations or not
    vm_traces: Option<bool>,
}

impl<'a> HookCreationParams<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hook_address: Address,
        account_balances: &'a HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &'a HashMap<Bytes, Token>,
        state: UniswapV4State,
        attributes: &'a HashMap<String, Bytes>,
        balances: &'a HashMap<Bytes, Bytes>,
        vm_traces: Option<bool>,
    ) -> Self {
        Self { hook_address, account_balances, all_tokens, state, attributes, balances, vm_traces }
    }

    /// The address of the hook the pool was created with.
    pub fn hook_address(&self) -> Address {
        self.hook_address
    }
}

pub trait HookHandlerCreator: Send + Sync {
    fn instantiate_hook_handler(
        &self,
        params: HookCreationParams,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError>;
}

pub struct GenericVMHookHandlerCreator;

impl HookHandlerCreator for GenericVMHookHandlerCreator {
    fn instantiate_hook_handler(
        &self,
        params: HookCreationParams<'_>,
    ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
        let pool_manager_address_bytes = params
            .attributes
            .get("balance_owner")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("balance_owner".to_string()))?;

        let pool_manager_address = Address::from_slice(&pool_manager_address_bytes.0);

        let limits_entrypoint = params
            .attributes
            .get("limits_entrypoint")
            .and_then(|bytes| String::from_utf8(bytes.0.to_vec()).ok());

        let is_euler = params
            .attributes
            .get("hook_identifier")
            .and_then(|bytes| String::from_utf8(bytes.0.to_vec()).ok())
            .unwrap_or_default() ==
            "euler_v1";

        let mut trace = false;
        if let Some(vm_traces) = params.vm_traces {
            trace = vm_traces
        }

        let engine = create_engine(SHARED_TYCHO_DB.clone(), trace).map_err(|e| {
            InvalidSnapshotError::VMError(SimulationError::FatalError(format!(
                "Failed to create engine: {e:?}"
            )))
        })?;

        let external_account_info = AccountInfo {
            balance: U256::from(0),
            nonce: 0u64,
            code_hash: KECCAK_EMPTY,
            code: None,
        };

        engine
            .state
            .init_account(*EXTERNAL_ACCOUNT, external_account_info, None, true)
            .map_err(|err| {
                InvalidSnapshotError::VMError(SimulationError::FatalError(format!(
                    "Failed to init external account: {err:?}"
                )))
            })?;

        let hook_handler = GenericVMHookHandler::new(
            params.hook_address,
            engine,
            pool_manager_address,
            params.all_tokens.clone(),
            params.account_balances.clone(),
            limits_entrypoint,
            is_euler,
        )
        .map_err(InvalidSnapshotError::VMError)?;

        Ok(Box::new(hook_handler))
    }
}

/// Chains on which a hook with no registered native handler is simulated with the generic VM
/// handler.
///
/// Anywhere else an unregistered hook is rejected at decode time, so a pool whose hook can move
/// the price is never quoted as if the hook were absent.
pub const GENERIC_VM_HOOK_CHAINS: [Chain; 2] = [Chain::Ethereum, Chain::Unichain];

/// Angstrom's Uniswap V4 hook, deployed on Ethereum mainnet only.
const ANGSTROM_HOOK_ADDRESS: &str = "0x0000000aa232009084Bd71A5797d089AA4Edfad4";

/// Handler creators keyed by chain and hook address. The same hook address on two chains is two
/// different deployments, so the chain is part of the key.
type HookHandlerRegistry = HashMap<(Chain, Address), Box<dyn HookHandlerCreator>>;

// Workaround for stateless decoder trait.
static HANDLER_FACTORY: LazyLock<RwLock<HookHandlerRegistry>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

static DEFAULT_HANDLER: LazyLock<Box<dyn HookHandlerCreator>> =
    LazyLock::new(|| Box::new(GenericVMHookHandlerCreator {}));

/// Registers the hook handlers this crate ships with.
///
/// Calling it more than once is harmless: each registration replaces the previous one for the
/// same chain and hook.
pub fn initialize_hook_handlers() -> Result<(), SimulationError> {
    let angstrom_hook_address = Address::from_str(ANGSTROM_HOOK_ADDRESS).map_err(|_| {
        SimulationError::FatalError("Failed to parse Angstrom hook address".to_string())
    })?;
    // Angstrom's attributes are only produced by the Ethereum manifest, so the same address on
    // another chain is not an Angstrom pool.
    register_hook_handler(Chain::Ethereum, angstrom_hook_address, Box::new(AngstromHookCreator))?;

    // Pons V2 is deployed on Robinhood alone, and only the Robinhood manifest emits the
    // `pons_*` attributes its creator reads.
    register_hook_handler(Chain::Robinhood, PONS_V2_HOOK_ROBINHOOD, Box::new(PonsV2HookCreator))?;

    Ok(())
}

/// Registers `handler` as the native handler for `hook` on `chain`, replacing any previous
/// registration for that pair.
pub fn register_hook_handler(
    chain: Chain,
    hook: Address,
    handler: Box<dyn HookHandlerCreator>,
) -> Result<(), SimulationError> {
    HANDLER_FACTORY
        .write()
        .map_err(|e| SimulationError::FatalError(e.to_string()))?
        .insert((chain, hook), handler);
    Ok(())
}

/// Builds the [`HookHandler`] for `hook_address` on `chain`.
///
/// The native handler registered for the pair wins. With none registered, the generic VM handler
/// serves the chains in [`GENERIC_VM_HOOK_CHAINS`]; on every other chain the hook is unsupported
/// and this returns [`InvalidSnapshotError::ValueError`], so the pool fails to decode rather than
/// being quoted as if it had no hook.
pub fn instantiate_hook_handler(
    chain: Chain,
    hook_address: &Address,
    params: HookCreationParams<'_>,
) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
    let factory = HANDLER_FACTORY
        .read()
        .map_err(|e| InvalidSnapshotError::VMError(SimulationError::FatalError(e.to_string())))?;
    if let Some(creator) = factory.get(&(chain, *hook_address)) {
        return creator.instantiate_hook_handler(params);
    }
    if GENERIC_VM_HOOK_CHAINS.contains(&chain) {
        return DEFAULT_HANDLER.instantiate_hook_handler(params);
    }
    Err(InvalidSnapshotError::ValueError(format!(
        "unsupported uniswap v4 hook {hook_address} on {chain}: no native handler registered and \
         the generic VM hook handler is not enabled on this chain"
    )))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use rstest::rstest;

    use super::*;
    use crate::evm::protocol::uniswap_v4::state::UniswapV4Fees;

    /// Owns the collections a [`HookCreationParams`] borrows, so a test can build params for an
    /// arbitrary hook address with no attributes set. Every creator then fails on the first
    /// attribute it needs, which identifies the creator that ran.
    #[derive(Default)]
    struct BareSnapshot {
        account_balances: HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: HashMap<Bytes, Token>,
        attributes: HashMap<String, Bytes>,
        balances: HashMap<Bytes, Bytes>,
    }

    impl BareSnapshot {
        fn params(&self, hook: Address) -> HookCreationParams<'_> {
            let state = UniswapV4State::new(
                0,
                U256::from(1),
                UniswapV4Fees::new(0, 0, 0),
                0,
                1,
                Vec::new(),
            )
            .expect("bare pool state should build");
            HookCreationParams::new(
                hook,
                &self.account_balances,
                &self.all_tokens,
                state,
                &self.attributes,
                &self.balances,
                None,
            )
        }
    }

    /// A creator whose only job is to be recognisable in the error it returns.
    struct TestHookCreator;

    impl HookHandlerCreator for TestHookCreator {
        fn instantiate_hook_handler(
            &self,
            _params: HookCreationParams<'_>,
        ) -> Result<Box<dyn HookHandler>, InvalidSnapshotError> {
            Err(InvalidSnapshotError::ValueError("test creator".to_string()))
        }
    }

    fn angstrom_hook() -> Address {
        Address::from_str(ANGSTROM_HOOK_ADDRESS).expect("angstrom hook address should parse")
    }

    /// Instantiates `hook` on `chain` from a bare snapshot and returns the error every creator
    /// raises for want of attributes.
    fn instantiation_error(chain: Chain, hook: Address) -> InvalidSnapshotError {
        let snapshot = BareSnapshot::default();
        let Err(error) = instantiate_hook_handler(chain, &hook, snapshot.params(hook)) else {
            panic!("a bare snapshot carries no attributes, so no creator can succeed");
        };
        error
    }

    #[rstest]
    #[case::robinhood(Chain::Robinhood, "robinhood")]
    #[case::base(Chain::Base, "base")]
    fn unregistered_hook_is_rejected_off_the_generic_vm_chains(
        #[case] chain: Chain,
        #[case] chain_name: &str,
    ) {
        initialize_hook_handlers().expect("hook handler registration should succeed");
        let hook = address!("00000000000000000000000000000000000000c4");

        let error = instantiation_error(chain, hook);

        let InvalidSnapshotError::ValueError(message) = error else {
            panic!("expected the unsupported-hook error, got {error:?}");
        };
        assert!(message.contains("unsupported uniswap v4 hook"), "{message}");
        assert!(message.contains(chain_name), "{message}");
    }

    #[rstest]
    #[case::ethereum(Chain::Ethereum)]
    #[case::unichain(Chain::Unichain)]
    fn unregistered_hook_falls_back_to_the_generic_vm_handler(#[case] chain: Chain) {
        initialize_hook_handlers().expect("hook handler registration should succeed");
        let hook = address!("00000000000000000000000000000000000000c5");

        let error = instantiation_error(chain, hook);

        let InvalidSnapshotError::MissingAttribute(attribute) = error else {
            panic!("expected the generic VM creator to run, got {error:?}");
        };
        assert_eq!(attribute, "balance_owner");
    }

    #[test]
    fn angstrom_hook_uses_its_native_creator_on_ethereum() {
        initialize_hook_handlers().expect("hook handler registration should succeed");

        let error = instantiation_error(Chain::Ethereum, angstrom_hook());

        let InvalidSnapshotError::MissingAttribute(attribute) = error else {
            panic!("expected the Angstrom creator to run, got {error:?}");
        };
        assert_eq!(attribute, "hooks");
    }

    #[test]
    fn angstrom_hook_falls_back_to_the_generic_vm_handler_off_ethereum() {
        // Angstrom's attributes are only produced by the Ethereum manifest, so the creator is
        // registered for Ethereum alone and the same address elsewhere is just another hook.
        initialize_hook_handlers().expect("hook handler registration should succeed");

        let error = instantiation_error(Chain::Unichain, angstrom_hook());

        let InvalidSnapshotError::MissingAttribute(attribute) = error else {
            panic!("expected the generic VM creator to run, got {error:?}");
        };
        assert_eq!(attribute, "balance_owner");
    }

    #[test]
    fn a_registered_creator_serves_only_the_chain_it_was_registered_for() {
        initialize_hook_handlers().expect("hook handler registration should succeed");
        let hook = address!("00000000000000000000000000000000000000c6");
        register_hook_handler(Chain::Robinhood, hook, Box::new(TestHookCreator))
            .expect("registering a hook handler should succeed");

        let registered = instantiation_error(Chain::Robinhood, hook);
        let InvalidSnapshotError::ValueError(message) = registered else {
            panic!("expected the registered creator to run, got {registered:?}");
        };
        assert_eq!(message, "test creator");

        let elsewhere = instantiation_error(Chain::Ethereum, hook);
        let InvalidSnapshotError::MissingAttribute(attribute) = elsewhere else {
            panic!("expected the generic VM creator to run, got {elsewhere:?}");
        };
        assert_eq!(attribute, "balance_owner");
    }
}
