// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {FeeCalculator} from "@src/FeeCalculator.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {TychoRouterV3, ClientFeeParams} from "@src/TychoRouterV3.sol";
import "@src/executors/TychoSubsidizingExecutor.sol";

contract MintableToken is ERC20 {
    constructor() ERC20("Token", "TKN") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

/// @dev Pays out tokenOut 1:1 for tokenIn
contract MockPool {
    IERC20 public immutable tokenIn;
    IERC20 public immutable tokenOut;
    uint256 public reserveIn;

    constructor(IERC20 tokenIn_, IERC20 tokenOut_) {
        tokenIn = tokenIn_;
        tokenOut = tokenOut_;
    }

    /// @dev Expects amountIn to have arrived before the call
    function swap(uint256 amountIn, address receiver) external {
        _settle(amountIn, receiver);
    }

    /// @dev Pulls amountIn from the caller
    function swapPull(uint256 amountIn, address receiver) external {
        require(tokenIn.transferFrom(msg.sender, address(this), amountIn));
        _settle(amountIn, receiver);
    }

    function _settle(uint256 amountIn, address receiver) internal {
        uint256 balance = tokenIn.balanceOf(address(this));
        require(balance - reserveIn >= amountIn, "MockPool: underpaid");
        reserveIn = balance;
        require(tokenOut.transfer(receiver, amountIn));
    }
}

/// @dev Swap data: [pool: 20]
contract MockExecutor is IExecutor {
    TransferManager.TransferType public immutable transferType;

    constructor(TransferManager.TransferType transferType_) {
        transferType = transferType_;
    }

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        MockPool pool = MockPool(address(bytes20(data[0:20])));
        if (transferType == TransferManager.TransferType.ProtocolWillDebit) {
            pool.swapPull(amountIn, receiver);
        } else {
            pool.swap(amountIn, receiver);
        }
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (TransferManager.TransferType, address, address, address, bool)
    {
        MockPool pool = MockPool(address(bytes20(data[0:20])));
        return (
            transferType,
            address(pool),
            address(pool.tokenIn()),
            address(pool.tokenOut()),
            false
        );
    }

    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address)
    {
        return address(bytes20(data[0:20]));
    }
}

contract TychoSubsidizingExecutorTest is Test {
    address constant ADMIN = address(0xAD);
    address constant SUBSIDY_WALLET = address(0x5B);
    address constant USER = address(0xA11CE);
    address constant ATTACKER = address(0xB0B);
    uint256 constant SIGNER_KEY = 0x516;
    uint256 constant AMOUNT_IN = 1_000_000 ether;
    // AMOUNT_IN at 0.01 BPS
    uint256 constant SUBSIDY = 1 ether;
    uint256 constant VAULT_BALANCE = 100 ether;

    MintableToken tokenIn;
    MintableToken tokenOut;
    MockPool pool;
    FeeCalculator feeCalculator;
    TychoRouterV3 router;
    TychoSubsidizingExecutor executor;
    MockExecutor transferExecutor;
    MockExecutor pullExecutor;
    address inner;

    function setUp() public {
        tokenIn = new MintableToken();
        tokenOut = new MintableToken();
        pool = new MockPool(tokenIn, tokenOut);
        tokenOut.mint(address(pool), 10 * AMOUNT_IN);

        feeCalculator = new FeeCalculator(ADMIN, ADMIN);
        vm.startPrank(ADMIN);
        feeCalculator.setSubsidyWallet(SUBSIDY_WALLET);
        feeCalculator.setSubsidySigner(vm.addr(SIGNER_KEY));
        vm.stopPrank();

        // Permit2 only needs to be a contract here
        router = new TychoRouterV3(
            address(pool), address(feeCalculator), ADMIN, ADMIN, ADMIN, ADMIN
        );
        executor = new TychoSubsidizingExecutor();
        transferExecutor =
            new MockExecutor(TransferManager.TransferType.Transfer);
        pullExecutor =
            new MockExecutor(TransferManager.TransferType.ProtocolWillDebit);
        address[] memory executors = new address[](3);
        executors[0] = address(executor);
        executors[1] = address(transferExecutor);
        executors[2] = address(pullExecutor);
        vm.prank(ADMIN);
        router.setExecutors(executors);
        vm.warp(block.timestamp + 1 days);
        inner = address(transferExecutor);

        tokenIn.mint(SUBSIDY_WALLET, VAULT_BALANCE);
        vm.startPrank(SUBSIDY_WALLET);
        tokenIn.approve(address(router), VAULT_BALANCE);
        router.deposit(address(tokenIn), VAULT_BALANCE);
        vm.stopPrank();
        _fund(USER);
        _fund(ATTACKER);
    }

    function _fund(address account) internal {
        tokenIn.mint(account, 10 * AMOUNT_IN);
        vm.prank(account);
        tokenIn.approve(address(router), type(uint256).max);
    }

    function _walletVaultBalance() internal view returns (uint256) {
        return
            router.balanceOf(SUBSIDY_WALLET, uint256(uint160(address(tokenIn))));
    }

    function _sign(uint256 subsidy, uint256 nonce, uint256 deadline)
        internal
        view
        returns (bytes memory)
    {
        bytes32 domainSeparator = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,"
                    "address verifyingContract)"
                ),
                keccak256("TychoSubsidizingExecutor"),
                keccak256("1"),
                block.chainid,
                address(router)
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(
                executor.SUBSIDY_TYPEHASH(),
                address(executor),
                address(tokenIn),
                subsidy,
                nonce,
                deadline
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(
            SIGNER_KEY,
            keccak256(abi.encodePacked("\x19\x01", domainSeparator, structHash))
        );
        return abi.encodePacked(r, s, v);
    }

    function _swapData(uint256 subsidy, uint256 nonce, bytes memory signature)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodePacked(
            address(executor),
            // Test values fit the encoded widths.
            // forge-lint: disable-next-line(unsafe-typecast)
            uint128(subsidy),
            nonce,
            // forge-lint: disable-next-line(unsafe-typecast)
            uint48(block.timestamp + 60),
            signature,
            inner,
            address(pool)
        );
    }

    function _signedSwapData(uint256 nonce)
        internal
        view
        returns (bytes memory)
    {
        return _swapData(
            SUBSIDY, nonce, _sign(SUBSIDY, nonce, block.timestamp + 60)
        );
    }

    /// @dev The quote includes the subsidy, so positive slippage capture
    ///      leaves it with the user.
    function _swap(address sender, uint256 amountIn, bytes memory swapData)
        internal
        returns (uint256)
    {
        ClientFeeParams memory noClientFee;
        vm.prank(sender);
        return router.singleSwap(
            amountIn,
            address(tokenIn),
            address(tokenOut),
            amountIn + SUBSIDY,
            amountIn,
            sender,
            noClientFee,
            swapData
        );
    }

    function _expectUnapproved() internal {
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__UnapprovedInnerExecutor.selector,
                inner
            )
        );
    }

    function testSubsidyAddedToInputFromVault() public {
        uint256 amountOut = _swap(USER, AMOUNT_IN, _signedSwapData(1));

        assertEq(amountOut, AMOUNT_IN + SUBSIDY);
        assertEq(tokenOut.balanceOf(USER), AMOUNT_IN + SUBSIDY);
        assertEq(_walletVaultBalance(), VAULT_BALANCE - SUBSIDY);
        assertEq(tokenIn.balanceOf(address(router)), _walletVaultBalance());
    }

    function testSubsidyAddedToInputOfPullingPool() public {
        inner = address(pullExecutor);

        uint256 amountOut = _swap(USER, AMOUNT_IN, _signedSwapData(1));

        assertEq(amountOut, AMOUNT_IN + SUBSIDY);
        assertEq(tokenIn.balanceOf(address(router)), _walletVaultBalance());
        assertEq(tokenIn.allowance(address(router), address(pool)), 0);
    }

    function testCopiedSignatureSkipsSubsidyOnOriginalSwap() public {
        bytes memory swapData = _signedSwapData(1);
        _swap(ATTACKER, AMOUNT_IN, swapData);

        uint256 amountOut = _swap(USER, AMOUNT_IN, swapData);

        assertEq(amountOut, AMOUNT_IN);
        assertEq(_walletVaultBalance(), VAULT_BALANCE - SUBSIDY);
    }

    function testCopiedSignatureOnSmallTradeReverts() public {
        bytes memory swapData = _signedSwapData(1);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__SubsidyTooHigh.selector,
                SUBSIDY,
                SUBSIDY / 2
            )
        );
        _swap(ATTACKER, AMOUNT_IN / 2, swapData);
    }

    function testSubsidyAboveFeeCalculatorCapReverts() public {
        vm.prank(ADMIN);
        feeCalculator.setSubsidyBps(50);
        bytes memory swapData = _signedSwapData(1);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__SubsidyTooHigh.selector,
                SUBSIDY,
                SUBSIDY / 2
            )
        );
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testInsufficientVaultBalanceReverts() public {
        vm.prank(SUBSIDY_WALLET);
        router.withdraw(address(tokenIn), VAULT_BALANCE - SUBSIDY / 2);
        bytes memory swapData = _signedSwapData(1);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__InsufficientVaultBalance.selector,
                SUBSIDY,
                SUBSIDY / 2
            )
        );
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testNoncesInSameBitmapWordAreIndependent() public {
        _swap(USER, AMOUNT_IN, _signedSwapData(1));
        _swap(USER, AMOUNT_IN, _signedSwapData(2));

        assertEq(tokenOut.balanceOf(USER), 2 * (AMOUNT_IN + SUBSIDY));
    }

    function testChangedSubsidyReverts() public {
        bytes memory swapData =
            _swapData(SUBSIDY / 2, 1, _sign(SUBSIDY, 1, block.timestamp + 60));

        vm.expectRevert(TychoSubsidizingExecutor__InvalidSignature.selector);
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testRotatedSignerRevertsOldSignature() public {
        bytes memory swapData = _signedSwapData(1);
        vm.prank(ADMIN);
        feeCalculator.setSubsidySigner(address(0xD00D));

        vm.expectRevert(TychoSubsidizingExecutor__InvalidSignature.selector);
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testExpiredSignatureReverts() public {
        bytes memory swapData = _signedSwapData(1);
        vm.warp(block.timestamp + 61);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__ExpiredSignature.selector,
                block.timestamp - 1,
                block.timestamp
            )
        );
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testUnsetSignerDisablesSubsidy() public {
        bytes memory swapData = _signedSwapData(1);
        vm.prank(ADMIN);
        feeCalculator.setSubsidySigner(address(0));

        vm.expectRevert(TychoSubsidizingExecutor__SubsidyDisabled.selector);
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testUnapprovedInnerExecutorReverts() public {
        inner = address(new MockExecutor(TransferManager.TransferType.Transfer));
        bytes memory swapData = _signedSwapData(1);

        _expectUnapproved();
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testTimelockedInnerExecutorReverts() public {
        inner = address(new MockExecutor(TransferManager.TransferType.Transfer));
        address[] memory executors = new address[](1);
        executors[0] = inner;
        vm.prank(ADMIN);
        router.setExecutors(executors);
        bytes memory swapData = _signedSwapData(1);

        _expectUnapproved();
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testSubsidizingExecutorAsInnerExecutorReverts() public {
        inner = address(executor);
        bytes memory swapData = _signedSwapData(1);

        _expectUnapproved();
        _swap(USER, AMOUNT_IN, swapData);
    }

    function testCallbackInnerExecutorReverts() public {
        inner = address(new MockExecutor(TransferManager.TransferType.None));
        address[] memory executors = new address[](1);
        executors[0] = inner;
        vm.prank(ADMIN);
        router.setExecutors(executors);
        vm.warp(block.timestamp + 1 days);
        bytes memory swapData = _signedSwapData(1);

        vm.expectRevert(
            TychoSubsidizingExecutor__UnsupportedInnerExecutor.selector
        );
        _swap(USER, AMOUNT_IN, swapData);
    }
}
