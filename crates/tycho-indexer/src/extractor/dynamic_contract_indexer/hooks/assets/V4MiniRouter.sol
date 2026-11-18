// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "v4-periphery/src/V4Router.sol";
import {console} from "forge-std/console.sol";

contract V4MiniRouter is V4Router {
    constructor(IPoolManager _poolManager) V4Router(_poolManager) {}

    function msgSender() public view override returns (address) {
        return address(this);
    }

    function _pay(Currency token, address payer, uint256 amount) internal override {
        poolManager.burn(payer, token.toId(), amount);
    }

    function execute(bytes calldata params) public {
        _executeActions(params);
    }

    receive() external payable {
        // Uniswap V4 pools sometimes send native currency to routers during swaps
        // This no-op function allows the contract to accept those transfers
    }
}
