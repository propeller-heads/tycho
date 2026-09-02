pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "@src/executors/BiconomyExecutor.sol";
import {Constants} from "../Constants.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {TransferManager} from "../../src/TransferManager.sol";

contract BiconomyExecutorExposed is BiconomyExecutor {
    constructor(address _biconomyAdapter) BiconomyExecutor(_biconomyAdapter) {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            address tokenIn,
            address tokenOut,
            bytes memory commitData,
            IBiconomyAdapter.FillLeg[] memory legs
        )
    {
        return _decodeData(data);
    }
}

contract MintableERC20 is ERC20 {
    constructor(string memory name_, string memory symbol_)
        ERC20(name_, symbol_)
    {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

/// @dev Local stand-in for the PropAMM adapter (the real one lives on Base).
///      Pulls exactly amountIn from the caller like the real adapter, records
///      every argument for assertions and mints amountOutToDeliver of
///      tokenOut to the receiver.
contract MockPropAMMAdapter is IBiconomyAdapter {
    using SafeERC20 for IERC20;

    uint256 public amountOutToDeliver;

    address public lastTokenIn;
    address public lastTokenOut;
    uint256 public lastAmountIn;
    uint256 public lastMinOut;
    address public lastReceiver;
    bytes public lastCommitData;
    uint256 public lastLegCount;
    bytes32 public lastLegsHash;

    constructor(uint256 amountOutToDeliver_) {
        amountOutToDeliver = amountOutToDeliver_;
    }

    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minOut,
        address receiver,
        bytes calldata commitData,
        FillLeg[] calldata legs
    ) external returns (uint256 delivered) {
        lastTokenIn = tokenIn;
        lastTokenOut = tokenOut;
        lastAmountIn = amountIn;
        lastMinOut = minOut;
        lastReceiver = receiver;
        lastCommitData = commitData;
        lastLegCount = legs.length;
        lastLegsHash = keccak256(abi.encode(legs));

        IERC20(tokenIn).safeTransferFrom(msg.sender, address(this), amountIn);
        MintableERC20(tokenOut).mint(receiver, amountOutToDeliver);
        return amountOutToDeliver;
    }
}

contract BiconomyExecutorTest is Constants, TestUtils {
    BiconomyExecutorExposed propammExecutor;
    MockPropAMMAdapter mockAdapter;
    MintableERC20 tokenIn;
    MintableERC20 tokenOut;

    uint256 constant AMOUNT_IN = 15 ether;
    uint256 constant AMOUNT_OUT = 28165000000;
    address constant PROVIDER =
        address(0x1111111111111111111111111111111111111111);
    address constant MAKER =
        address(0x2222222222222222222222222222222222222222);

    function setUp() public {
        mockAdapter = new MockPropAMMAdapter(AMOUNT_OUT);
        propammExecutor = new BiconomyExecutorExposed(address(mockAdapter));
        tokenIn = new MintableERC20("Wrapped Ether", "WETH");
        tokenOut = new MintableERC20("USD Coin", "USDC");
    }

    function _sampleLegs()
        internal
        view
        returns (IBiconomyAdapter.FillLeg[] memory legs)
    {
        IBiconomyAdapter.Level[] memory levels = new IBiconomyAdapter.Level[](2);
        // Cumulative sizes in tokenIn wei, 1e18-scaled prices
        levels[0] = IBiconomyAdapter.Level({size: 10 ether, price: 1878e6});
        levels[1] = IBiconomyAdapter.Level({size: 20 ether, price: 1877e6});

        legs = new IBiconomyAdapter.FillLeg[](1);
        legs[0] = IBiconomyAdapter.FillLeg({
            ladder: IBiconomyAdapter.PriceLadder({
                mm: MAKER,
                provider: PROVIDER,
                tokenIn: address(tokenIn),
                tokenOut: address(tokenOut),
                levels: levels,
                nonce: 7,
                expiresAt: 1751536030
            }),
            amountIn: AMOUNT_IN
        });
    }

    function _encodeSwapData(IBiconomyAdapter.FillLeg[] memory legs)
        internal
        view
        returns (bytes memory)
    {
        bytes memory commitData = abi.encodeWithSignature(
            "updatePrices(bytes)", hex"deadbeef"
        );
        return abi.encode(address(tokenIn), address(tokenOut), commitData, legs);
    }

    function testDecodeData() public view {
        IBiconomyAdapter.FillLeg[] memory legs = _sampleLegs();
        bytes memory commitData =
            abi.encodeWithSignature("updatePrices(bytes)", hex"deadbeef");
        bytes memory data =
            abi.encode(address(tokenIn), address(tokenOut), commitData, legs);

        (
            address decodedTokenIn,
            address decodedTokenOut,
            bytes memory decodedCommitData,
            IBiconomyAdapter.FillLeg[] memory decodedLegs
        ) = propammExecutor.decodeData(data);

        assertEq(decodedTokenIn, address(tokenIn), "tokenIn mismatch");
        assertEq(decodedTokenOut, address(tokenOut), "tokenOut mismatch");
        assertEq(
            keccak256(decodedCommitData),
            keccak256(commitData),
            "commitData mismatch"
        );
        assertEq(decodedLegs.length, 1, "legs length mismatch");
        assertEq(
            decodedLegs[0].ladder.provider,
            legs[0].ladder.provider,
            "provider mismatch"
        );
        assertEq(decodedLegs[0].amountIn, AMOUNT_IN, "leg amountIn mismatch");
        assertEq(decodedLegs[0].ladder.mm, legs[0].ladder.mm, "mm mismatch");
        assertEq(
            decodedLegs[0].ladder.tokenIn,
            address(tokenIn),
            "ladder tokenIn mismatch"
        );
        assertEq(
            decodedLegs[0].ladder.tokenOut,
            address(tokenOut),
            "ladder tokenOut mismatch"
        );
        assertEq(decodedLegs[0].ladder.nonce, 7, "nonce mismatch");
        assertEq(
            decodedLegs[0].ladder.expiresAt, 1751536030, "expiresAt mismatch"
        );
        assertEq(
            decodedLegs[0].ladder.levels.length, 2, "levels length mismatch"
        );
        assertEq(
            decodedLegs[0].ladder.levels[0].size,
            10 ether,
            "level 0 size mismatch"
        );
        assertEq(
            decodedLegs[0].ladder.levels[0].price,
            1878e6,
            "level 0 price mismatch"
        );
        assertEq(
            decodedLegs[0].ladder.levels[1].size,
            20 ether,
            "level 1 size mismatch"
        );
        assertEq(
            decodedLegs[0].ladder.levels[1].price,
            1877e6,
            "level 1 price mismatch"
        );
    }

    function testDecodeDataInvalidLength() public {
        // Shorter than the minimal abi.encode(address, address, bytes,
        // FillLeg[]) payload of 192 bytes
        bytes memory tooShort = new bytes(191);
        vm.expectRevert(
            BiconomyExecutor.BiconomyExecutor__InvalidDataLength.selector
        );
        propammExecutor.decodeData(tooShort);
    }

    function testGetTransferData() public view {
        bytes memory data = _encodeSwapData(_sampleLegs());

        (
            TransferManager.TransferType transferType,
            address receiver,
            address decodedTokenIn,
            address decodedTokenOut,
            bool outputToRouter
        ) = propammExecutor.getTransferData(data);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit),
            "transferType mismatch"
        );
        assertEq(receiver, address(mockAdapter), "receiver mismatch");
        assertEq(decodedTokenIn, address(tokenIn), "tokenIn mismatch");
        assertEq(decodedTokenOut, address(tokenOut), "tokenOut mismatch");
        assertEq(outputToRouter, false, "outputToRouter mismatch");
    }

    function testGetTransferDataInvalidLength() public {
        bytes memory tooShort = new bytes(191);
        vm.expectRevert(
            BiconomyExecutor.BiconomyExecutor__InvalidDataLength.selector
        );
        propammExecutor.getTransferData(tooShort);
    }

    function testSwap() public {
        IBiconomyAdapter.FillLeg[] memory legs = _sampleLegs();
        bytes memory commitData =
            abi.encodeWithSignature("updatePrices(bytes)", hex"deadbeef");
        bytes memory data =
            abi.encode(address(tokenIn), address(tokenOut), commitData, legs);

        tokenIn.mint(address(propammExecutor), AMOUNT_IN);
        // In production the router's TransferManager approves the adapter
        // before the executor runs (ProtocolWillDebit); stand in for it here.
        vm.prank(address(propammExecutor));
        tokenIn.approve(address(mockAdapter), AMOUNT_IN);

        propammExecutor.swap(AMOUNT_IN, data, BOB);

        // The adapter pulled exactly amountIn from the executor
        assertEq(
            tokenIn.balanceOf(address(propammExecutor)),
            0,
            "tokenIn left in executor"
        );
        assertEq(
            tokenIn.balanceOf(address(mockAdapter)),
            AMOUNT_IN,
            "tokenIn should be at adapter"
        );
        // The TransferManager-style approval was fully consumed
        assertEq(
            tokenIn.allowance(address(propammExecutor), address(mockAdapter)),
            0,
            "unconsumed allowance"
        );
        // Output was delivered straight to the receiver
        assertEq(
            tokenOut.balanceOf(BOB),
            AMOUNT_OUT,
            "tokenOut should be at receiver"
        );

        // All arguments were forwarded to the adapter untouched
        assertEq(mockAdapter.lastTokenIn(), address(tokenIn), "tokenIn arg");
        assertEq(mockAdapter.lastTokenOut(), address(tokenOut), "tokenOut arg");
        assertEq(mockAdapter.lastAmountIn(), AMOUNT_IN, "amountIn arg");
        assertEq(mockAdapter.lastMinOut(), 0, "minOut must be zero");
        assertEq(mockAdapter.lastReceiver(), BOB, "receiver arg");
        assertEq(
            keccak256(mockAdapter.lastCommitData()),
            keccak256(commitData),
            "commitData arg"
        );
        assertEq(mockAdapter.lastLegCount(), 1, "leg count");
        assertEq(
            mockAdapter.lastLegsHash(), keccak256(abi.encode(legs)), "legs arg"
        );
    }

    function testSwapInvalidDataLength() public {
        bytes memory tooShort = new bytes(191);
        vm.expectRevert(
            BiconomyExecutor.BiconomyExecutor__InvalidDataLength.selector
        );
        propammExecutor.swap(AMOUNT_IN, tooShort, BOB);
    }

    function testConstructorZeroAddress() public {
        vm.expectRevert(BiconomyExecutor.BiconomyExecutor__ZeroAddress.selector);
        new BiconomyExecutorExposed(address(0));
    }

    function testFundsExpectedAddress() public view {
        bytes memory data = _encodeSwapData(_sampleLegs());
        // Called with a regular call, so msg.sender (this test) is returned:
        // funds must be at the caller (the router in production).
        assertEq(
            propammExecutor.fundsExpectedAddress(data),
            address(this),
            "fundsExpectedAddress mismatch"
        );
    }
}
