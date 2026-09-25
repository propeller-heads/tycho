// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;
import "../TychoRouterTestSetup.sol";
import {TesseraExecutor} from "../../src/executors/TesseraExecutor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

interface ITesseraQuote {
    function tesseraSwapViewAmounts(address, address, int256)
        external
        view
        returns (uint256, uint256);
}

contract TesseraRouterTest is TychoRouterTestSetup {
    address constant VENUE = 0x55555522005BcAE1c2424D474BfD5ed477749E3e;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 50_548_423;
    }

    function test_router_settles_view_quote_with_empty_tag() public {
        uint256 amount = 0.01 ether;
        deal(BASE_WETH, ALICE, amount);
        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, amount);
        // The venue sees the router as its caller; use identical quote context.
        vm.stopPrank();
        vm.prank(tychoRouterAddr);
        (, uint256 quote) = ITesseraQuote(VENUE)
            .tesseraSwapViewAmounts(BASE_WETH, BASE_USDC, int256(amount));
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_tessera_weth_usdc");
        assertEq(protocolData, abi.encodePacked(BASE_WETH, BASE_USDC));
        vm.expectCall(
            VENUE,
            abi.encodeWithSignature(
                "tesseraSwapWithAllowances(address,address,int256,uint256,address,bytes)",
                BASE_WETH,
                BASE_USDC,
                int256(amount),
                uint256(0),
                ALICE,
                bytes("")
            )
        );
        uint256 beforeBalance = IERC20(BASE_USDC).balanceOf(ALICE);
        vm.prank(ALICE);
        uint256 received = tychoRouter.singleSwap(
            amount,
            BASE_WETH,
            BASE_USDC,
            quote,
            quote,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(tesseraExecutor), protocolData)
        );
        assertEq(received, quote);
        assertEq(IERC20(BASE_USDC).balanceOf(ALICE) - beforeBalance, quote);
        emit log_named_address(
            "Tessera executor test address", address(tesseraExecutor)
        );
    }
}
