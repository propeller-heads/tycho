pragma solidity ^0.8.26;

import {ClientFeeParams} from "@src/TychoRouterV3.sol";
import {Vault__InsufficientBalance} from "@src/Vault.sol";
import {ERC1271Wallet} from "./ClientFeeTestHelper.sol";
import "./TychoRouterTestSetup.sol";

/**
 * @dev Covers the single-use property of a contribution authorization:
 *      for one client and one contributionNonce, at most one swap succeeds.
 */
contract TychoRouterClientFeeNonceTest is TychoRouterTestSetup {
    uint256 private constant _AMOUNT_IN = 1 ether;
    // Above the pool output of 1 WETH -> DAI, so the client covers a shortfall.
    uint256 private constant _SHORTFALL_MIN_OUT = 2020 * 1e18;
    // Below the pool output, so the swap needs no contribution.
    uint256 private constant _NO_SHORTFALL_MIN_OUT = 2000 * 1e18;
    uint256 private constant _MAX_CONTRIBUTION = 20 * 1e18;

    event ClientContributionNoncesInvalidated(
        address indexed client, uint248 indexed wordPos, uint256 mask
    );

    /**
     * @dev ERC-6909 vault id of the contribution token.
     */
    function _daiVaultId() private view returns (uint256) {
        return uint256(uint160(DAI_ADDR));
    }

    /**
     * @dev Encoded single Uniswap V2 swap of WETH for DAI.
     */
    function _swapData() private view returns (bytes memory) {
        return encodeSingleSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
    }

    /**
     * @dev Gives ALICE the input token for `swapCount` swaps and approves the
     *      router to take all of it.
     */
    function _fundUser(uint256 swapCount) private {
        uint256 total = _AMOUNT_IN * swapCount;
        deal(WETH_ADDR, ALICE, total);
        vm.prank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, total);
    }

    /**
     * @dev Deposits `amount` DAI into `client`'s vault balance, the inventory a
     *      contribution debits.
     */
    function _fundClientVault(address client, uint256 amount) private {
        deal(DAI_ADDR, client, amount);
        vm.startPrank(client);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amount);
        tychoRouter.deposit(DAI_ADDR, amount);
        vm.stopPrank();
    }

    function _signedParams(
        address client,
        uint256 maxClientContribution,
        uint256 contributionNonce,
        uint256 minAmountOut,
        uint256 deadline,
        uint256 signerPk
    ) private view returns (ClientFeeParams memory params) {
        params = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: client,
            maxClientContribution: maxClientContribution,
            contributionNonce: contributionNonce,
            deadline: deadline,
            clientSignature: new bytes(0)
        });
        params.clientSignature = signClientFee(
            params,
            _AMOUNT_IN,
            WETH_ADDR,
            DAI_ADDR,
            minAmountOut,
            minAmountOut,
            ALICE,
            _swapData(),
            tychoRouterAddr,
            signerPk
        );
    }

    /**
     * @dev A contribution authorization by ALICE, signed with her own key.
     */
    function _contributionParams(
        uint256 contributionNonce,
        uint256 minAmountOut
    ) private view returns (ClientFeeParams memory) {
        return _signedParams(
            ALICE,
            _MAX_CONTRIBUTION,
            contributionNonce,
            minAmountOut,
            block.timestamp + 1 hours,
            ALICE_PK
        );
    }

    function _singleSwap(ClientFeeParams memory params, uint256 minAmountOut)
        private
        returns (uint256)
    {
        return tychoRouter.singleSwap(
            _AMOUNT_IN,
            WETH_ADDR,
            DAI_ADDR,
            minAmountOut,
            minAmountOut,
            ALICE,
            params,
            _swapData()
        );
    }

    function _bitmapWord(address client, uint256 contributionNonce)
        private
        view
        returns (uint256)
    {
        return tychoRouter.clientContributionNonceBitmap(
            client, uint248(contributionNonce >> 8)
        );
    }

    function _isUsed(address client, uint256 contributionNonce)
        private
        view
        returns (bool)
    {
        uint256 bit = 1 << uint8(contributionNonce);
        return _bitmapWord(client, contributionNonce) & bit != 0;
    }

    function testContributionNonceConsumedOnSuccessfulSwap() public {
        _fundUser(1);
        _fundClientVault(ALICE, _MAX_CONTRIBUTION);

        ClientFeeParams memory params =
            _contributionParams(7, _SHORTFALL_MIN_OUT);
        assertFalse(_isUsed(ALICE, 7));

        vm.prank(ALICE);
        uint256 amountOut = _singleSwap(params, _SHORTFALL_MIN_OUT);

        assertEq(amountOut, _SHORTFALL_MIN_OUT);
        assertTrue(_isUsed(ALICE, 7));
    }

    function testReplayOfContributionAuthorizationReverts() public {
        _fundUser(2);
        // Twice the contribution, so only the nonce can block the replay.
        _fundClientVault(ALICE, 2 * _MAX_CONTRIBUTION);

        ClientFeeParams memory params =
            _contributionParams(0, _SHORTFALL_MIN_OUT);

        vm.startPrank(ALICE);
        _singleSwap(params, _SHORTFALL_MIN_OUT);
        uint256 vaultBalance = tychoRouter.balanceOf(ALICE, _daiVaultId());

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__InvalidClientContributionNonce.selector, ALICE, 0
            )
        );
        _singleSwap(params, _SHORTFALL_MIN_OUT);
        vm.stopPrank();

        assertEq(tychoRouter.balanceOf(ALICE, _daiVaultId()), vaultBalance);
    }

    function testContributionNoncesExecuteOutOfOrder() public {
        _fundUser(2);
        _fundClientVault(ALICE, 2 * _MAX_CONTRIBUTION);

        ClientFeeParams memory later =
            _contributionParams(5, _SHORTFALL_MIN_OUT);
        ClientFeeParams memory earlier =
            _contributionParams(2, _SHORTFALL_MIN_OUT);

        vm.startPrank(ALICE);
        assertEq(_singleSwap(later, _SHORTFALL_MIN_OUT), _SHORTFALL_MIN_OUT);
        assertEq(_singleSwap(earlier, _SHORTFALL_MIN_OUT), _SHORTFALL_MIN_OUT);
        vm.stopPrank();

        assertTrue(_isUsed(ALICE, 5));
        assertTrue(_isUsed(ALICE, 2));
    }

    function testSuccessfulSwapConsumesNonceWithoutContributing() public {
        _fundUser(2);
        _fundClientVault(ALICE, _MAX_CONTRIBUTION);

        ClientFeeParams memory params =
            _contributionParams(3, _NO_SHORTFALL_MIN_OUT);
        uint256 vaultBalance = tychoRouter.balanceOf(ALICE, _daiVaultId());

        vm.startPrank(ALICE);
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);

        // The pool covered the whole quote, so nothing was debited.
        assertGe(tychoRouter.balanceOf(ALICE, _daiVaultId()), vaultBalance);
        assertTrue(_isUsed(ALICE, 3));

        // A later shortfall must not resurrect the authorization.
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__InvalidClientContributionNonce.selector, ALICE, 3
            )
        );
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);
        vm.stopPrank();
    }

    function testExpiredAuthorizationDoesNotConsumeNonce() public {
        _fundUser(1);
        _fundClientVault(ALICE, _MAX_CONTRIBUTION);

        ClientFeeParams memory params = _signedParams(
            ALICE,
            _MAX_CONTRIBUTION,
            9,
            _SHORTFALL_MIN_OUT,
            block.timestamp - 1,
            ALICE_PK
        );

        vm.startPrank(ALICE);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__ExpiredClientSignature.selector,
                block.timestamp - 1,
                block.timestamp
            )
        );
        _singleSwap(params, _SHORTFALL_MIN_OUT);
        vm.stopPrank();

        assertFalse(_isUsed(ALICE, 9));
    }

    function testInvalidSignatureDoesNotConsumeNonce() public {
        _fundUser(1);
        _fundClientVault(ALICE, _MAX_CONTRIBUTION);

        // ALICE is the client, but somebody else signed.
        ClientFeeParams memory params = _signedParams(
            ALICE,
            _MAX_CONTRIBUTION,
            11,
            _SHORTFALL_MIN_OUT,
            block.timestamp + 1 hours,
            CLIENT_FEE_RECEIVER_PK
        );

        vm.startPrank(ALICE);
        vm.expectRevert(TychoRouter__InvalidClientSignature.selector);
        _singleSwap(params, _SHORTFALL_MIN_OUT);
        vm.stopPrank();

        assertFalse(_isUsed(ALICE, 11));
    }

    function testDownstreamRevertRollsBackNonceConsumption() public {
        _fundUser(2);

        ClientFeeParams memory params =
            _contributionParams(13, _SHORTFALL_MIN_OUT);

        // The client vault is empty, so covering the shortfall reverts.
        vm.prank(ALICE);
        vm.expectPartialRevert(Vault__InsufficientBalance.selector);
        _singleSwap(params, _SHORTFALL_MIN_OUT);

        assertFalse(_isUsed(ALICE, 13));

        // The same authorization still works once the inventory is there.
        _fundClientVault(ALICE, _MAX_CONTRIBUTION);
        vm.prank(ALICE);
        assertEq(_singleSwap(params, _SHORTFALL_MIN_OUT), _SHORTFALL_MIN_OUT);
        assertTrue(_isUsed(ALICE, 13));
    }

    function testFeeOnlyAuthorizationRejectsNonZeroNonce() public {
        _fundUser(1);

        ClientFeeParams memory params = _signedParams(
            ALICE,
            0,
            4,
            _NO_SHORTFALL_MIN_OUT,
            block.timestamp + 1 hours,
            ALICE_PK
        );

        vm.prank(ALICE);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NonZeroContributionNonce.selector, 4
            )
        );
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);
    }

    function testFeeOnlyAuthorizationLeavesBitmapUntouched() public {
        _fundUser(2);

        ClientFeeParams memory params = _signedParams(
            ALICE,
            0,
            0,
            _NO_SHORTFALL_MIN_OUT,
            block.timestamp + 1 hours,
            ALICE_PK
        );

        vm.startPrank(ALICE);
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);
        assertEq(_bitmapWord(ALICE, 0), 0);

        // A fee-only authorization stays replayable.
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);
        assertEq(_bitmapWord(ALICE, 0), 0);
        vm.stopPrank();
    }

    function testZeroClientRejectsNonZeroNonce() public {
        _fundUser(1);

        ClientFeeParams memory params = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: address(0),
            maxClientContribution: 0,
            contributionNonce: 1,
            deadline: 0,
            clientSignature: new bytes(0)
        });

        vm.prank(ALICE);
        vm.expectRevert(TychoRouter__AddressZero.selector);
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);
    }

    function testZeroClientRejectsDeadlineAndSignature() public {
        _fundUser(2);

        ClientFeeParams memory params = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: address(0),
            maxClientContribution: 0,
            contributionNonce: 0,
            deadline: block.timestamp + 1 hours,
            clientSignature: new bytes(0)
        });

        vm.prank(ALICE);
        vm.expectRevert(TychoRouter__AddressZero.selector);
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);

        params.deadline = 0;
        params.clientSignature = new bytes(65);

        vm.prank(ALICE);
        vm.expectRevert(TychoRouter__AddressZero.selector);
        _singleSwap(params, _NO_SHORTFALL_MIN_OUT);
    }

    function testInvalidationBlocksAnUnusedAuthorization() public {
        _fundUser(1);
        _fundClientVault(ALICE, _MAX_CONTRIBUTION);

        ClientFeeParams memory params =
            _contributionParams(300, _SHORTFALL_MIN_OUT);

        vm.prank(ALICE);
        tychoRouter.invalidateClientContributionNonces(1, 1 << 44);

        assertTrue(_isUsed(ALICE, 300));

        vm.prank(ALICE);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__InvalidClientContributionNonce.selector, ALICE, 300
            )
        );
        _singleSwap(params, _SHORTFALL_MIN_OUT);
    }

    function testInvalidationEmitsEventAndCannotClearBits() public {
        vm.startPrank(ALICE);
        vm.expectEmit();
        emit ClientContributionNoncesInvalidated(ALICE, 0, 3);
        tychoRouter.invalidateClientContributionNonces(0, 3);
        assertEq(_bitmapWord(ALICE, 0), 3);

        // Repeating the call is idempotent, and a zero mask clears nothing.
        tychoRouter.invalidateClientContributionNonces(0, 3);
        tychoRouter.invalidateClientContributionNonces(0, 0);
        assertEq(_bitmapWord(ALICE, 0), 3);
        vm.stopPrank();
    }

    function testInvalidationWorksWhilePaused() public {
        vm.prank(PAUSER);
        tychoRouter.pause();

        vm.prank(ALICE);
        tychoRouter.invalidateClientContributionNonces(0, 1);

        assertTrue(_isUsed(ALICE, 0));
    }

    function testInvalidationOnlyTouchesTheCallerNamespace() public {
        vm.prank(ALICE);
        tychoRouter.invalidateClientContributionNonces(0, 1);

        assertTrue(_isUsed(ALICE, 0));
        assertFalse(_isUsed(clientFeeReceiver, 0));
    }

    function testERC1271ClientCannotReplayContribution() public {
        ERC1271Wallet wallet =
            new ERC1271Wallet(vm.addr(CLIENT_FEE_RECEIVER_PK));
        _fundUser(2);
        _fundClientVault(address(wallet), 2 * _MAX_CONTRIBUTION);

        ClientFeeParams memory params = _signedParams(
            address(wallet),
            _MAX_CONTRIBUTION,
            21,
            _SHORTFALL_MIN_OUT,
            block.timestamp + 1 hours,
            CLIENT_FEE_RECEIVER_PK
        );

        vm.startPrank(ALICE);
        assertEq(_singleSwap(params, _SHORTFALL_MIN_OUT), _SHORTFALL_MIN_OUT);
        assertTrue(_isUsed(address(wallet), 21));

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__InvalidClientContributionNonce.selector,
                address(wallet),
                21
            )
        );
        _singleSwap(params, _SHORTFALL_MIN_OUT);
        vm.stopPrank();
    }

    function testNoncesAcrossAWordBoundaryDoNotCollide() public {
        vm.startPrank(ALICE);
        tychoRouter.invalidateClientContributionNonces(0, 1 << 255);
        vm.stopPrank();

        assertTrue(_isUsed(ALICE, 255));
        assertFalse(_isUsed(ALICE, 254));
        assertFalse(_isUsed(ALICE, 256));
    }

    function testFuzzEachNonceMapsToOneBit(uint256 contributionNonce) public {
        uint248 wordPos = uint248(contributionNonce >> 8);
        uint256 bit = 1 << uint8(contributionNonce);

        vm.prank(ALICE);
        tychoRouter.invalidateClientContributionNonces(wordPos, bit);

        assertEq(tychoRouter.clientContributionNonceBitmap(ALICE, wordPos), bit);
        assertTrue(_isUsed(ALICE, contributionNonce));
        // Every other nonce in the word stays free.
        assertEq(
            tychoRouter.clientContributionNonceBitmap(ALICE, wordPos) & ~bit, 0
        );
    }
}
