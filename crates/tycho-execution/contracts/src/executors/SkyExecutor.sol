// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

error SkyExecutor__InvalidDataLength();
error SkyExecutor__InvalidTarget();
error SkyExecutor__InvalidDirection();
error SkyExecutor__TokenMismatch();

// Shared subset of DssLitePsm and UsdsPsmWrapper (the wrapper exposes the same
// compatibility getters; its dai() returns USDS).
interface IDssLitePsm {
    function dai() external view returns (address);
    function gem() external view returns (address);
    function tout() external view returns (uint256);
    function to18ConversionFactor() external view returns (uint256);
    function sellGem(address usr, uint256 gemAmt)
        external
        returns (uint256 daiOutWad);
    function buyGem(address usr, uint256 gemAmt)
        external
        returns (uint256 daiInWad);
}

interface IDaiUsds {
    function dai() external view returns (address);
    function usds() external view returns (address);
    function daiToUsds(address usr, uint256 wad) external;
    function usdsToDai(address usr, uint256 wad) external;
}

/// The three Sky venues and their tokens are fixed at deployment, so swap data
/// is just two bytes: a target selector and an `isGemToStable` direction flag
/// (mirroring tycho-simulation's SkyState semantics — the gem is USDC on the
/// PSM legs and USDS on the converter). No caller-controlled addresses.
contract SkyExecutor is IExecutor {
    uint256 private constant _WAD = 1e18;

    enum Target {
        Psm, // DssLitePsm: USDC (gem) <-> DAI
        Wrapper, // UsdsPsmWrapper: USDC (gem) <-> USDS
        Converter // DaiUsds: USDS (gem) <-> DAI
    }

    IDssLitePsm private immutable _PSM;
    IDssLitePsm private immutable _WRAPPER;
    IDaiUsds private immutable _CONVERTER;
    address private immutable _DAI;
    address private immutable _USDC;
    address private immutable _USDS;
    uint256 private immutable _PSM_FACTOR;
    uint256 private immutable _WRAPPER_FACTOR;

    constructor(address psm_, address wrapper_, address converter_) {
        _PSM = IDssLitePsm(psm_);
        _WRAPPER = IDssLitePsm(wrapper_);
        _CONVERTER = IDaiUsds(converter_);
        _DAI = _CONVERTER.dai();
        _USDS = _CONVERTER.usds();
        _USDC = _PSM.gem();
        _PSM_FACTOR = _PSM.to18ConversionFactor();
        _WRAPPER_FACTOR = _WRAPPER.to18ConversionFactor();
        // The three venues must agree on the token set they are wired for.
        if (
            _PSM.dai() != _DAI || _WRAPPER.gem() != _USDC
                || _WRAPPER.dai() != _USDS
        ) {
            revert SkyExecutor__TokenMismatch();
        }
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (Target target, bool gemToStable) = _decodeData(data);

        if (target == Target.Converter) {
            if (gemToStable) {
                _CONVERTER.usdsToDai(receiver, amountIn);
            } else {
                _CONVERTER.daiToUsds(receiver, amountIn);
            }
            return;
        }

        (IDssLitePsm psm, uint256 factor) = target == Target.Psm
            ? (_PSM, _PSM_FACTOR)
            : (_WRAPPER, _WRAPPER_FACTOR);
        if (gemToStable) {
            // slither-disable-next-line unused-return
            psm.sellGem(receiver, amountIn);
        } else {
            // buyGem takes the gem OUTPUT amount and pulls the stable cost
            // (incl. tout) from the caller. Rounding down guarantees the cost
            // never exceeds amountIn; at most factor-1 wei of the stable stays
            // unspent at the router. A HALTED tout (uint256.max) reverts on
            // the checked addition, matching the venue's halt semantics.
            uint256 gemAmt = (amountIn * _WAD) / (factor * (_WAD + psm.tout()));
            // slither-disable-next-line unused-return
            psm.buyGem(receiver, gemAmt);
        }
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (Target target, bool gemToStable) = _decodeData(data);
        // All Sky venues pull tokenIn from the caller via allowance and honor a
        // recipient parameter for the output.
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;

        address stable;
        address gem;
        if (target == Target.Psm) {
            (stable, gem, receiver) = (_DAI, _USDC, address(_PSM));
        } else if (target == Target.Wrapper) {
            (stable, gem, receiver) = (_USDS, _USDC, address(_WRAPPER));
        } else {
            (stable, gem, receiver) = (_DAI, _USDS, address(_CONVERTER));
        }
        (tokenIn, tokenOut) = gemToStable ? (gem, stable) : (stable, gem);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (Target target, bool gemToStable)
    {
        if (data.length != 2) {
            revert SkyExecutor__InvalidDataLength();
        }
        uint8 rawTarget = uint8(data[0]);
        if (rawTarget > uint8(type(Target).max)) {
            revert SkyExecutor__InvalidTarget();
        }
        target = Target(rawTarget);
        uint8 rawDirection = uint8(data[1]);
        if (rawDirection > 1) {
            revert SkyExecutor__InvalidDirection();
        }
        gemToStable = rawDirection == 1;
    }
}
