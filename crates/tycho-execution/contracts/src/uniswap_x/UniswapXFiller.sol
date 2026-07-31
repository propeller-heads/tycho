// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "./IReactor.sol";
import "./IReactorCallback.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/access/AccessControl.sol";
import "@openzeppelin/contracts/utils/Address.sol";
import {TychoRouter} from "../TychoRouter.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error UniswapXFiller__AddressZero();
error UniswapXFiller__BatchExecutionNotSupported();

contract UniswapXFiller is AccessControl, IReactorCallback {
    using SafeERC20 for IERC20;

    // UniswapX V2DutchOrder Reactor
    IReactor public immutable reactor;
    address public immutable tychoRouter;
    address public immutable nativeAddress;

    // keccak256("NAME_OF_ROLE") : save gas on deployment
    bytes32 public constant REACTOR_ROLE =
        0x39dd1d7269516fc1f719706a5e9b05cdcb1644978808b171257d9a8eab55dd57;
    bytes32 public constant EXECUTOR_ROLE =
        0xd8aa0f3194971a2a116679f7c2090f6939c8d4e01a2a8d7e41d55e5351469e63;

    event Withdrawal(
        address indexed token, uint256 amount, address indexed receiver
    );

    constructor(
        address _tychoRouter,
        address _reactor,
        address _native_address
    ) {
        if (_tychoRouter == address(0)) {
            revert UniswapXFiller__AddressZero();
        }
        if (_reactor == address(0)) revert UniswapXFiller__AddressZero();

        _grantRole(DEFAULT_ADMIN_ROLE, msg.sender);
        _grantRole(REACTOR_ROLE, address(_reactor));
        tychoRouter = _tychoRouter;
        reactor = IReactor(_reactor);

        // slither-disable-next-line missing-zero-check
        nativeAddress = _native_address;
    }

    function execute(SignedOrder calldata order, bytes calldata callbackData)
        external
        onlyRole(EXECUTOR_ROLE)
    {
        reactor.executeWithCallback(order, callbackData);
    }

    function reactorCallback(
        ResolvedOrder[] calldata resolvedOrders,
        bytes calldata callbackData
    ) external onlyRole(REACTOR_ROLE) {
        if (resolvedOrders.length != 1) {
            revert UniswapXFiller__BatchExecutionNotSupported();
        }

        ResolvedOrder memory order = resolvedOrders[0];

        bool tokenInApprovalNeeded = bool(uint8(callbackData[0]) == 1);
        bool tokenOutApprovalNeeded = bool(uint8(callbackData[1]) == 1);
        bytes calldata tychoCalldata = bytes(callbackData[2:]);

        // The TychoRouter will take the input tokens from the filler
        if (tokenInApprovalNeeded) {
            // Native ETH input is not supported by UniswapX
            IERC20(order.input.token)
                .forceApprove(tychoRouter, type(uint256).max);
        }

        // slither-disable-next-line low-level-calls
        (bool success, bytes memory result) = tychoRouter.call(tychoCalldata);

        if (!success) {
            revert(
                string(
                    result.length > 0
                        ? result
                        : abi.encodePacked("Execution failed")
                )
            );
        }

        if (tokenOutApprovalNeeded) {
            // Multiple outputs are possible when taking fees - but token itself should
            // not change.
            OutputToken memory output = order.outputs[0];
            if (output.token != nativeAddress) {
                IERC20 token = IERC20(output.token);
                token.forceApprove(address(reactor), type(uint256).max);
            } else {
                // With native ETH - the filler is responsible for transferring back
                // to the reactor.
                Address.sendValue(payable(address(reactor)), output.amount);
            }
        }
    }

    /**
     * @dev Allows granting roles to multiple accounts in a single call.
     */
    function batchGrantRole(bytes32 role, address[] memory accounts)
        external
        onlyRole(DEFAULT_ADMIN_ROLE)
    {
        for (uint256 i = 0; i < accounts.length; i++) {
            _grantRole(role, accounts[i]);
        }
    }

    /**
     * @dev Allows withdrawing any ERC20 funds.
     */
    function withdraw(IERC20[] memory tokens, address receiver)
        external
        onlyRole(DEFAULT_ADMIN_ROLE)
    {
        if (receiver == address(0)) {
            revert UniswapXFiller__AddressZero();
        }

        for (uint256 i = 0; i < tokens.length; i++) {
            // slither-disable-next-line calls-loop
            uint256 tokenBalance = tokens[i].balanceOf(address(this));
            if (tokenBalance > 0) {
                emit Withdrawal(address(tokens[i]), tokenBalance, receiver);
                tokens[i].safeTransfer(receiver, tokenBalance);
            }
        }
    }

    /**
     * @dev Allows withdrawing any NATIVE funds.
     */
    function withdrawNative(address receiver)
        external
        onlyRole(DEFAULT_ADMIN_ROLE)
    {
        if (receiver == address(0)) {
            revert UniswapXFiller__AddressZero();
        }

        uint256 amount = address(this).balance;
        if (amount > 0) {
            emit Withdrawal(ETH_ADDRESS, amount, receiver);
            Address.sendValue(payable(receiver), amount);
        }
    }

    /**
     * @dev Allows this contract to receive native token with empty msg.data from contracts
     */
    // slither-disable-next-line locked-ether
    receive() external payable {
        require(msg.sender.code.length != 0);
    }
}
