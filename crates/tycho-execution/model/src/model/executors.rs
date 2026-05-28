//! Executors are modeled as variants of an [Executor] enum.
//! Functions like [Executor::swap] implement each [Executor]'s functionality
//! via pattern matching.
//!
//! While having an executor trait that is implemented by individual executor types
//! would keep each executor's code closer together,
//! it would require passing executors around as trait objects.
//! Trait objects require dynamic dispatch at runtime,
//! which is more costly than a pattern match,
//! prevents compiler optimizations like inlining,
//! and prevents CPU optimizations like branch prediction.
//! Since we want to execute millions of simulations per second,
//! pattern matching over enum variants was chosen.
//! If performance mattered less, trait objects would likely be chosen.
//!
//! <https://github.com/propeller-heads/tycho-indexer/tree/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors>
use serde::Serialize;

use crate::{
    address::Address,
    error::Error,
    log::Log,
    model::{
        dispatcher::_call_handle_callback_on_executor, transfer_manager::TransferType, vault::Vault,
    },
    params::{ParamKey, Params},
    state::State,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
pub enum Executor {
    // Only executors that give the caller control over the called pool contract were modeled.
    // When a new executor that fulfills these criteria is added, it needs to be modelled here
    // too.
    Curve,
    ERC4626,
    FluidV1,
    MaverickV2,
    Slipstreams,
    UniswapV2,
    UniswapV3,
    NativeWrap,
    AerodromeV1,
    LiquidityParty,
}

/// Return value of [Executor::get_transfer_data]
#[derive(Serialize, Clone)]
pub struct TransferData {
    pub transfer_type: TransferType,
    pub receiver: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub output_to_router: bool,
}

/// Return value of [Executor::get_callback_transfer_data]
pub struct CallbackTransferData {
    pub transfer_type: TransferType,
    pub receiver: Address,
}

impl Executor {
    /// Array containing all [Executor]s.
    pub const VARIANTS: [Executor; 10] = [
        Executor::Curve,
        Executor::ERC4626,
        Executor::FluidV1,
        Executor::MaverickV2,
        Executor::Slipstreams,
        Executor::UniswapV2,
        Executor::UniswapV3,
        Executor::NativeWrap,
        Executor::AerodromeV1,
        Executor::LiquidityParty,
    ];

    /// <https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/interfaces/IExecutor.sol#L41>
    pub fn get_transfer_data(
        &self,
        params: &Params,
        _state: &mut State,
        swap_index: u8,
    ) -> Result<TransferData, Error> {
        match self {
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/CurveExecutor.sol#L143
            Self::Curve => {
                let token_in = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?;
                let token_out = params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?;

                let transfer_type = if token_in == Address::NativeETH {
                    TransferType::TransferNativeInExecutor
                } else {
                    TransferType::ProtocolWillDebit
                };

                Ok(TransferData {
                    transfer_type,
                    receiver: params.request(
                        ParamKey::ProtocolData { swap_index, start: 40, end: 60 },
                        // trying more variants might find some very obscure bugs
                        // in the future but slows down simulation a lot
                        // and currently is ignored anyway
                        Address::SENDER_CONTROLLED,
                    )?,
                    token_in,
                    token_out,
                    output_to_router: true,
                })
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/ERC4626Executor.sol#L67
            Self::ERC4626 => {
                let token_in = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?;
                let receiver = params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;

                let is_redeem = token_in == receiver;

                let token_out = if is_redeem {
                    params.request(
                        ParamKey::SwapIndexed { prefix: "IERC4626.asset()", swap_index },
                        Address::POSSIBLY_ERC20_AND_NATIVE,
                    )?
                } else {
                    receiver
                };

                Ok(TransferData {
                    transfer_type: TransferType::ProtocolWillDebit,
                    receiver,
                    token_in,
                    token_out,
                    output_to_router: false,
                })
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/FluidV1Executor.sol#L134
            Self::FluidV1 => {
                let is_native_sell = params.request(
                    ParamKey::ProtocolData { swap_index, start: 61, end: 62 },
                    [true, false],
                )?;
                Ok(TransferData {
                    transfer_type: if is_native_sell {
                        TransferType::TransferNativeInExecutor
                    } else {
                        TransferType::None
                    },
                    receiver: Address::Zero,
                    token_in: if is_native_sell {
                        Address::NativeETH
                    } else {
                        params.request(
                            ParamKey::ProtocolData { swap_index, start: 21, end: 41 },
                            Address::POSSIBLY_ERC20_AND_NATIVE,
                        )?
                    },
                    token_out: params.request(
                        ParamKey::ProtocolData { swap_index, start: 41, end: 61 },
                        Address::POSSIBLY_ERC20_AND_NATIVE,
                    )?,
                    output_to_router: false,
                })
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/MaverickV2Executor.sol#L64
            Self::MaverickV2 => Ok(TransferData {
                transfer_type: TransferType::Transfer,
                receiver: params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?,
                token_in: params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                token_out: params.request(
                    ParamKey::ProtocolData { swap_index, start: 40, end: 60 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                output_to_router: false,
            }),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/SlipstreamsExecutor.sol#L83
            Self::Slipstreams => Ok(TransferData {
                transfer_type: TransferType::None,
                receiver: Address::Zero,
                token_in: params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                token_out: params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                output_to_router: false,
            }),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV2Executor.sol#L102
            Self::UniswapV2 => Ok(TransferData {
                transfer_type: TransferType::Transfer,
                receiver: params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?,
                token_in: params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                token_out: params.request(
                    ParamKey::ProtocolData { swap_index, start: 40, end: 60 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                output_to_router: false,
            }),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV3Executor.sol#L81
            Self::UniswapV3 => Ok(TransferData {
                transfer_type: TransferType::None,
                receiver: Address::Zero,
                token_in: params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                token_out: params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                output_to_router: false,
            }),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/NativeWrapExecutor.sol#L77
            Self::NativeWrap => {
                let is_wrapping = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 1 },
                    [true, false],
                )?;
                Ok(TransferData {
                    transfer_type: if is_wrapping {
                        TransferType::TransferNativeInExecutor
                    } else {
                        TransferType::ProtocolWillDebit
                    },
                    receiver: Address::Router,
                    token_in: if is_wrapping { Address::NativeETH } else { Address::WETH },
                    token_out: if is_wrapping { Address::WETH } else { Address::NativeETH },
                    output_to_router: true,
                })
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/AerodromeV1Executor.sol#L80
            Self::AerodromeV1 => Ok(TransferData {
                transfer_type: TransferType::Transfer,
                receiver: params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?,
                token_in: params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                token_out: params.request(
                    ParamKey::ProtocolData { swap_index, start: 40, end: 60 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                output_to_router: false,
            }),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/LiquidityPartyExecutor.sol#L34
            Self::LiquidityParty => Ok(TransferData {
                transfer_type: TransferType::Transfer,
                receiver: params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?,
                token_in: params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                token_out: params.request(
                    ParamKey::ProtocolData { swap_index, start: 40, end: 60 },
                    Address::POSSIBLY_ERC20_AND_NATIVE,
                )?,
                output_to_router: false,
            }),
        }
    }

    /// <https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/interfaces/IExecutor.sol#L23>
    #[allow(clippy::too_many_arguments)]
    pub fn swap(
        &self,
        params: &Params,
        state: &mut State,
        vault: &mut Vault,
        log: &mut impl Log,
        amount: i64,
        _receiver: Address,
        swap_index: u8,
    ) -> Result<(), Error> {
        match self {
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/CurveExecutor.sol#L71
            Self::Curve => {
                let pool = params.request(
                    ParamKey::ProtocolData { swap_index, start: 40, end: 60 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;

                if !pool.is_sender_controlled() {
                    return Err(Error::Ignore {
                        reason: "curve pool not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    });
                }

                let token_in = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    Address::VARIANTS,
                )?;

                // this simulates the transfer of eth to the pool
                if token_in == Address::NativeETH {
                    state.eth_send_value(Address::Router, pool, amount)?;
                }

                // if the sender controls the pool,
                // the actual swap logic doesn't matter

                let transfer_allowances_during_swap = params.request(
                    ParamKey::SwapIndexed { prefix: "transfer_allowances_during_swap", swap_index },
                    [true, false],
                )?;
                if transfer_allowances_during_swap {
                    for token in Address::VARIANTS {
                        for spender in Address::SENDER_CONTROLLED {
                            let allowance =
                                state.erc20_allowance(token, Address::Router, spender)?;
                            if allowance > 0 {
                                state.erc20_safe_transfer_from(
                                    token,
                                    spender,
                                    Address::Router,
                                    spender,
                                    allowance,
                                )?;
                            }
                        }
                    }
                }
                Ok(())
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/ERC4626Executor.sol#L32
            Self::ERC4626 => {
                let target = params.request(
                    ParamKey::ProtocolData { swap_index, start: 20, end: 40 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if target.is_sender_controlled() {
                    // if the sender controls the pool,
                    // the actual swap logic doesn't matter
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "erc4626 target not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/FluidV1Executor.sol#L61
            Self::FluidV1 => {
                let dex = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if !dex.is_sender_controlled() {
                    return Err(Error::Ignore {
                        reason: "fluidv1 dex not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    });
                }
                let is_native_sell = params.request(
                    ParamKey::ProtocolData { swap_index, start: 61, end: 62 },
                    [true, false],
                )?;
                if !is_native_sell {
                    state.tstore("fluid_v1_current_dex", dex);
                    state.enter_callback(dex);
                    _call_handle_callback_on_executor(params, state, vault, log, swap_index)?;
                    state.leave_callback();
                } else {
                    state.eth_send_value(Address::Router, dex, amount)?;
                }
                Ok(())
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/MaverickV2Executor.sol#L28
            Self::MaverickV2 => {
                let pool = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if pool.is_sender_controlled() {
                    // if the sender controls the pool,
                    // the actual swap logic doesn't matter
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "maverickv2 pool not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/SlipstreamsExecutor.sol#L37
            Self::Slipstreams => {
                let pool = params.request(
                    ParamKey::ProtocolData { swap_index, start: 43, end: 63 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if pool.is_sender_controlled() {
                    state.enter_callback(pool);
                    _call_handle_callback_on_executor(params, state, vault, log, swap_index)?;
                    state.leave_callback();
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "slipstreams pool not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV2Executor.sol#L39
            Self::UniswapV2 => {
                let pool = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if pool.is_sender_controlled() {
                    // if the sender controls the pool,
                    // the actual swap logic doesn't matter
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "uniswapv2 pool not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV3Executor.sol#L37
            Self::UniswapV3 => {
                let target = params.request(
                    ParamKey::ProtocolData { swap_index, start: 43, end: 63 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if target.is_sender_controlled() {
                    // if the sender controls the pool,
                    // the actual swap logic doesn't matter
                    // TODO theoretically it doesn't have to be the target that does the callback
                    state.enter_callback(target);
                    _call_handle_callback_on_executor(params, state, vault, log, swap_index)?;
                    state.leave_callback();
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "uniswapv3 target not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/NativeWrapExecutor.sol#L45
            Self::NativeWrap => {
                let is_wrapping = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 1 },
                    [true, false],
                )?;
                if is_wrapping {
                    state.eth_send_value(Address::Router, Address::WETH, amount)?;
                    state.erc20_safe_transfer(
                        Address::WETH,
                        Address::WETH,
                        Address::Router,
                        amount,
                    )?;
                } else {
                    state.erc20_safe_transfer(
                        Address::WETH,
                        Address::Router,
                        Address::WETH,
                        amount,
                    )?;
                    state.eth_send_value(Address::WETH, Address::Router, amount)?;
                }
                Ok(())
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/AerodromeV1Executor.sol#L32
            Self::AerodromeV1 => {
                let pool = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if pool.is_sender_controlled() {
                    // if the sender controls the pool,
                    // the actual swap logic doesn't matter
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "aerodrome v1 pool not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/LiquidityPartyExecutor.sol#L11
            Self::LiquidityParty => {
                let pool = params.request(
                    ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                    // trying more variants might find some very obscure bugs
                    // in the future but slows down simulation a lot
                    // and currently is ignored anyway
                    Address::SENDER_CONTROLLED,
                )?;
                if pool.is_sender_controlled() {
                    // if the sender controls the pool,
                    // the actual swap logic doesn't matter
                    Ok(())
                } else {
                    Err(Error::Ignore {
                        reason: "liquidity party pool not sender controlled. not low hanging fruit. would require simulating real pool".into(),
                    })
                }
            }
        }
    }

    pub fn get_callback_transfer_data(
        &self,
        _params: &Params,
        state: &State,
        _swap_index: u8,
    ) -> Result<CallbackTransferData, Error> {
        match self {
            Self::Curve => unimplemented!(),
            Self::ERC4626 => unimplemented!(),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/FluidV1Executor.sol#L159
            Self::FluidV1 => Ok(CallbackTransferData {
                transfer_type: TransferType::Transfer,
                receiver: Address::Named("fluid-v1-liquidity"),
            }),
            Self::MaverickV2 => unimplemented!(),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/SlipstreamsExecutor.sol#L107
            Self::Slipstreams => Ok(CallbackTransferData {
                transfer_type: TransferType::Transfer,
                // called via delegatecall. therefore not the router
                receiver: state.msg_sender(),
            }),
            Self::UniswapV2 => unimplemented!(),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV3Executor.sol#L105
            Self::UniswapV3 => Ok(CallbackTransferData {
                transfer_type: TransferType::Transfer,
                // called via delegatecall. therefore not the router
                receiver: state.msg_sender(),
            }),
            Self::NativeWrap => unimplemented!(),
            Self::AerodromeV1 => unimplemented!(),
            Self::LiquidityParty => unimplemented!(),
        }
    }

    pub fn handle_callback(&self, _params: &Params, state: &mut State) -> Result<(), Error> {
        match self {
            Self::Curve => unimplemented!("Curve doesn't use callbacks"),
            Self::ERC4626 => unimplemented!("ERC4626 doesn't use callbacks"),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/FluidV1Executor.sol#L117
            Self::FluidV1 => {
                let dex: Address = state.tload("fluid_v1_current_dex")?;
                if state.msg_sender() != dex {
                    return Err(Error::revert("FluidV1.handle_callback: msg.sender != dex"));
                }
                Ok(())
            }
            Self::MaverickV2 => unimplemented!("MaverickV2 doesn't use callbacks"),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/SlipstreamsExecutor.sol#L60
            // not worth modeling as it has no reverts or side effects
            Self::Slipstreams => Ok(()),
            Self::UniswapV2 => unimplemented!("UniswapV2 doesn't use callbacks"),
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV3Executor.sol#L58
            // not worth modeling as it has no reverts or side effects
            Self::UniswapV3 => Ok(()),
            Self::NativeWrap => unimplemented!("Wrap doesn't use callbacks"),
            Self::AerodromeV1 => unimplemented!("AerodromeV1 doesn't use callbacks"),
            Self::LiquidityParty => unimplemented!("LiquidityParty doesn't use callbacks"),
        }
    }

    /// <https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/interfaces/IExecutor.sol#L63>
    ///
    /// Most executors return `msg.sender` which translates to the router
    /// because the function is called via staticcall.
    pub fn funds_expected_address(
        &self,
        params: &Params,
        _state: &mut State,
        swap_index: u8,
    ) -> Result<Address, Error> {
        Ok(match self {
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/CurveExecutor.sol#L60
            Self::Curve => Address::Router,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/ERC4626Executor.sol#L21
            Self::ERC4626 => Address::Router,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/FluidV1Executor.sol#L50
            Self::FluidV1 => Address::Router,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/MaverickV2Executor.sol#L18
            Self::MaverickV2 => params.request(
                ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                // trying more variants might find some very obscure bugs
                // in the future but slows down simulation a lot
                // and currently is ignored anyway
                Address::SENDER_CONTROLLED,
            )?,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/SlipstreamsExecutor.sol#L26
            Self::Slipstreams => Address::Router,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV2Executor.sol#L29
            Self::UniswapV2 => params.request(
                ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                // trying more variants might find some very obscure bugs
                // in the future but slows down simulation a lot
                // and currently is ignored anyway
                Address::SENDER_CONTROLLED,
            )?,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/UniswapV3Executor.sol#L26
            Self::UniswapV3 => Address::Router,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/NativeWrapExecutor.sol#L34
            Self::NativeWrap => Address::Router,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/AerodromeV1Executor.sol#L23
            Self::AerodromeV1 => params.request(
                ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                // trying more variants might find some very obscure bugs
                // in the future but slows down simulation a lot
                // and currently is ignored anyway
                Address::SENDER_CONTROLLED,
            )?,
            // https://github.com/propeller-heads/tycho-indexer/blob/d0a5db4ab55baf9ff87fb54cdfb59e015866b409/crates/tycho-execution/contracts/src/executors/LiquidityPartyExecutor.sol#L54
            Self::LiquidityParty => params.request(
                ParamKey::ProtocolData { swap_index, start: 0, end: 20 },
                // trying more variants might find some very obscure bugs
                // in the future but slows down simulation a lot
                // and currently is ignored anyway
                Address::SENDER_CONTROLLED,
            )?,
        })
    }
}
