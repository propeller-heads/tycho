pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    MetricExecutor,
    MetricExecutor__InvalidCallback,
    MetricExecutor__InvalidDataLength
} from "@src/executors/MetricExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";

error MetricExecutorTest__UnexpectedTransferType();
error MetricDispatcherHarness__NoExecutor();
error MetricDispatcherHarness__UnexpectedTransferType();

interface IMetricSwapCallback {
    function metricOmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;

    function coolAmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

contract MetricToken is ERC20 {
    constructor(string memory name, string memory symbol) ERC20(name, symbol) {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}

contract MockMetricOracle {
    bool public updated;

    function updateBySignature(
        address, /* feedCreator */
        uint256, /* slotId */
        uint256, /* deadline */
        bytes calldata /* signature */
    )
        external
    {
        updated = true;
    }
}

contract MockMetricPool {
    address public immutable token0;
    address public immutable token1;
    MockMetricOracle public immutable oracle;
    bool public immutable requireOracleUpdate;

    constructor(
        address token0_,
        address token1_,
        MockMetricOracle oracle_,
        bool requireOracleUpdate_
    ) {
        token0 = token0_;
        token1 = token1_;
        oracle = oracle_;
        requireOracleUpdate = requireOracleUpdate_;
    }

    function swap(
        address receiver,
        bool zeroForOne,
        int128 amountSpecified,
        uint128, /* priceLimitX64 */
        bytes calldata data
    ) external {
        if (requireOracleUpdate) {
            require(oracle.updated(), "oracle not updated");
        }

        uint256 amountIn = uint256(uint128(amountSpecified));
        uint256 amountOut = amountIn * 2;

        if (zeroForOne) {
            IMetricSwapCallback(msg.sender)
                .metricOmmSwapCallback(
                    int256(amountIn), -int256(amountOut), data
                );
            IERC20(token1).transfer(receiver, amountOut);
        } else {
            IMetricSwapCallback(msg.sender)
                .metricOmmSwapCallback(
                    -int256(amountOut), int256(amountIn), data
                );
            IERC20(token0).transfer(receiver, amountOut);
        }
    }
}

contract MetricExecutorExposed is MetricExecutor {
    function metricOmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        _payCallback(amount0Delta, amount1Delta, data);
    }

    function coolAmmSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        _payCallback(amount0Delta, amount1Delta, data);
    }

    function _payCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) internal {
        address tokenIn = amount0Delta > 0
            ? MockMetricPool(msg.sender).token0()
            : MockMetricPool(msg.sender).token1();
        uint256 amountOwed =
            amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        if (transferType != TransferManager.TransferType.Transfer) {
            revert MetricExecutorTest__UnexpectedTransferType();
        }

        IERC20(tokenIn).transfer(receiver, amountOwed);
        handleCallback(msg.data);
        data;
    }
}

contract MetricDispatcherHarness {
    address public currentExecutor;

    function execute(
        address executor,
        uint256 amountIn,
        bytes calldata data,
        address receiver
    ) external {
        currentExecutor = executor;
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(
                IExecutor.swap.selector, amountIn, data, receiver
            )
        );
        currentExecutor = address(0);
        require(success, string(result));
    }

    fallback(bytes calldata data) external returns (bytes memory) {
        address executor = currentExecutor;
        if (executor == address(0)) {
            revert MetricDispatcherHarness__NoExecutor();
        }

        (bool transferDataSuccess, bytes memory transferData) = executor.staticcall(
            abi.encodeWithSelector(
                ICallback.getCallbackTransferData.selector,
                data,
                address(0),
                msg.sender
            )
        );
        require(transferDataSuccess, string(transferData));

        (TransferManager.TransferType transferType, address receiver) =
            abi.decode(transferData, (TransferManager.TransferType, address));
        if (transferType != TransferManager.TransferType.Transfer) {
            revert MetricDispatcherHarness__UnexpectedTransferType();
        }

        (int256 amount0Delta, int256 amount1Delta,) =
            abi.decode(data[4:], (int256, int256, bytes));
        address tokenIn = amount0Delta > 0
            ? MockMetricPool(msg.sender).token0()
            : MockMetricPool(msg.sender).token1();
        uint256 amountOwed =
            amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);

        IERC20(tokenIn).transfer(receiver, amountOwed);

        (bool callbackSuccess, bytes memory result) = executor.delegatecall(
            abi.encodeWithSelector(ICallback.handleCallback.selector, data)
        );
        require(callbackSuccess, string(result));
        return result;
    }
}

contract MetricExecutorTest is Test {
    MetricToken token0;
    MetricToken token1;
    MockMetricOracle oracle;
    MetricExecutorExposed executor;

    address receiver = makeAddr("receiver");
    address metricRouter = makeAddr("metricRouter");

    function setUp() public {
        token0 = new MetricToken("Token 0", "TK0");
        token1 = new MetricToken("Token 1", "TK1");
        oracle = new MockMetricOracle();
        executor = new MetricExecutorExposed();
    }

    function testGetTransferData() public {
        MockMetricPool pool = _pool(false);
        bytes memory data = _encodeData(address(pool), true, false, "");

        (
            TransferManager.TransferType transferType,
            address transferReceiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(data);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(transferReceiver, address(0));
        assertEq(tokenIn, address(token0));
        assertEq(tokenOut, address(token1));
        assertFalse(outputToRouter);
    }

    function testSwapWithoutOracleUpdate() public {
        MockMetricPool pool = _pool(false);
        uint256 amountIn = 100 ether;
        token0.mint(address(executor), amountIn);
        token1.mint(address(pool), amountIn * 2);

        executor.swap(
            amountIn, _encodeData(address(pool), true, false, ""), receiver
        );

        assertEq(token0.balanceOf(address(pool)), amountIn);
        assertEq(token1.balanceOf(receiver), amountIn * 2);
        assertEq(token0.balanceOf(address(executor)), 0);
    }

    function testSwapWithOracleUpdate() public {
        MockMetricPool pool = _pool(true);
        uint256 amountIn = 50 ether;
        token0.mint(address(executor), amountIn);
        token1.mint(address(pool), amountIn * 2);

        bytes memory oracleCalldata = abi.encodeCall(
            MockMetricOracle.updateBySignature,
            (address(this), uint256(0), uint256(block.timestamp + 1), "")
        );

        executor.swap(
            amountIn,
            _encodeData(address(pool), true, true, oracleCalldata),
            receiver
        );

        assertTrue(oracle.updated());
        assertEq(token1.balanceOf(receiver), amountIn * 2);
    }

    function testSwapThroughDispatcherHarness() public {
        MetricDispatcherHarness harness = new MetricDispatcherHarness();
        MetricExecutor implementation = new MetricExecutor();
        MockMetricPool pool = _pool(false);
        uint256 amountIn = 25 ether;
        token0.mint(address(harness), amountIn);
        token1.mint(address(pool), amountIn * 2);

        harness.execute(
            address(implementation),
            amountIn,
            _encodeData(address(pool), true, false, ""),
            receiver
        );

        assertEq(token0.balanceOf(address(pool)), amountIn);
        assertEq(token1.balanceOf(receiver), amountIn * 2);
        assertEq(token0.balanceOf(address(harness)), 0);
    }

    function testInvalidOracleDataLength() public {
        MockMetricPool pool = _pool(false);
        bytes memory invalid = abi.encodePacked(
            _baseData(address(pool), true),
            bytes1(uint8(1)),
            address(oracle),
            uint32(99),
            hex"1234"
        );

        vm.expectRevert(MetricExecutor__InvalidDataLength.selector);
        executor.getTransferData(invalid);
    }

    function testRejectsCallbackOutsideSwap() public {
        vm.expectRevert(MetricExecutor__InvalidCallback.selector);
        executor.handleCallback(
            abi.encodeWithSelector(
                IMetricSwapCallback.metricOmmSwapCallback.selector,
                int256(1),
                int256(0),
                ""
            )
        );
    }

    function _pool(bool requireOracleUpdate) internal returns (MockMetricPool) {
        return new MockMetricPool(
            address(token0), address(token1), oracle, requireOracleUpdate
        );
    }

    function _encodeData(
        address pool,
        bool zeroForOne,
        bool shouldUpdateOracle,
        bytes memory oracleCalldata
    ) internal view returns (bytes memory) {
        bytes memory data = abi.encodePacked(
            _baseData(pool, zeroForOne),
            bytes1(uint8(shouldUpdateOracle ? 1 : 0))
        );
        if (!shouldUpdateOracle) {
            return data;
        }

        return abi.encodePacked(
            data, address(oracle), uint32(oracleCalldata.length), oracleCalldata
        );
    }

    function _baseData(address pool, bool zeroForOne)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodePacked(
            address(token0),
            address(token1),
            pool,
            metricRouter,
            bytes1(uint8(zeroForOne ? 1 : 0)),
            bytes32(uint256(0))
        );
    }
}
