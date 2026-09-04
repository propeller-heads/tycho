pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";

// Polygon
address constant POLYGON_WETH = 0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619;
address constant POLYGON_USDC = 0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359;

// BNB Chain
address constant BSC_WBNB = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;

// Base
address constant BASE_USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
address constant BASE_MAG7 = 0x9E6A46f294bB67c20F1D1E7AfB0bBEf614403B55;
address constant BASE_WETH = 0x4200000000000000000000000000000000000006;
address constant BASE_cbBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;

// Roles and keys
bytes32 constant ROUTER_FEE_SETTER_ROLE =
    0x9939157be7760e9462f1d5a0dcad88b616ddc64138e317108b40b1cf55601348;
address constant ALICE = 0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2;
uint256 constant ALICE_PK =
    0x123456789abcdef123456789abcdef123456789abcdef123456789abcdef1234;

// Client fee receiver key pair — address is vm.addr(CLIENT_FEE_RECEIVER_PK)
uint256 constant CLIENT_FEE_RECEIVER_PK =
    0x6789abcdef1234567890abcdef1234567890abcdef1234567890abcdef123456;

// Assets
address constant ETH_ADDR = address(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE);
address constant WETH_ADDR =
    address(0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2);
address constant DAI_ADDR = address(0x6B175474E89094C44Da98b954EedeAC495271d0F);
address constant BAL_ADDR = address(0xba100000625a3754423978a60c9317c58a424e3D);
address constant USDC_ADDR =
    address(0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48);
address constant WBTC_ADDR =
    address(0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599);
address constant USDE_ADDR =
    address(0x4c9EDD5852cd905f086C759E8383e09bff1E68B3);
address constant USDT_ADDR =
    address(0xdAC17F958D2ee523a2206206994597C13D831ec7);
address constant PEPE_ADDR =
    address(0x6982508145454Ce325dDbE47a25d4ec3d2311933);
address constant STETH_ADDR =
    address(0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84);
address constant RETH_ADDR =
    address(0xae78736Cd615f374D3085123A210448E74Fc6393);

// Permit2
address constant PERMIT2_ADDRESS = 0x000000000022D473030F116dDEE9F6B43aC78BA3;

contract Constants is Test {
    address ADMIN = makeAddr("unique-admin"); //admin=us
    // Changed this since someone deployed a drainer contract to makeAddr("bob")
    address BOB = makeAddr("bob*"); //bob=someone!=us
    address EXECUTOR_SETTER = makeAddr("executorSetter");
    address FEE_SETTER = makeAddr("feeSetter");
    // Dummy contracts
    address DUMMY = makeAddr("dummy");
    address DUMMY2 = makeAddr("dummy2");
    address DUMMY3 = makeAddr("dummy3");
    address PAUSER = makeAddr("pauser");
    address UNPAUSER = makeAddr("unpauser");

    /**
     * @dev Deploys a dummy contract with non-empty bytecode
     */
    function deployDummyContract() internal {
        bytes memory minimalBytecode = hex"01"; // Single-byte bytecode
        // Deploy minimal bytecode
        vm.etch(DUMMY, minimalBytecode);
        vm.etch(DUMMY2, minimalBytecode);
        vm.etch(DUMMY3, minimalBytecode);
    }

    // Block offset for setup to ensure that the executors timelock has passed in the setup
    uint256 internal constant _SETUP_TIME_OFFSET_NEW_EXECUTOR = 7 days; // ~7 days
}
