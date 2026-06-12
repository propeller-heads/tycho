pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";

contract PolygonConstants {
    address constant POLYGON_WETH = 0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619;
    address constant POLYGON_USDC = 0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359;
}

contract ArbitrumConstants {
    address constant ARBITRUM_WETH = 0x82aF49447D8a07e3bd95BD0d56f35241523fBab1;
    address constant ARBITRUM_USDC = 0xaf88d065e77c8cC2239327C5EDb3A432268e5831;
}

contract BscConstants {
    address constant BSC_WBNB = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
    address constant BSC_WETH = 0x2170Ed0880ac9A755fd29B2688956BD959F933F8;
}

contract BaseConstants {
    address constant BASE_USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant BASE_MAG7 = 0x9E6A46f294bB67c20F1D1E7AfB0bBEf614403B55;
    address constant BASE_WETH = 0x4200000000000000000000000000000000000006;
    address constant BASE_cbBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant BASE_BMI = 0x2F16386bB37709016023232523FF6d9DAF444BE3;
    // Uniswap v2
    address constant USDC_MAG7_POOL =
        0x739c2431670A12E2cF8e11E3603eB96e6728a789;

    //Pancakeswap v3
    address constant PANCAKESWAPV3_cbBTC_USDC_POOL =
        0xb94b22332ABf5f89877A14Cc88f2aBC48c34B3Df;

    address constant PANCAKESWAPV3_DEPLOYER =
        0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9;

    // Aerodrome Slipstreams Factory
    address constant SLIPSTREAMS_FACTORY_BASE =
        0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A;
    address constant SLIPSTREAMS_NEW_FACTORY_BASE =
        0xaDe65c38CD4849aDBA595a4323a8C7DdfE89716a;
    // Aerodrome Slipstreams
    address constant SLIPSTREAMS_WETH_USDC_POOL =
        0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59;
    // Pool that uses the new factory
    address constant SLIPSTREAMS_WETH_BMI_POOL =
        0x1b7088833548A8C4Ab12a900a2f4bdFA49221081;
}

contract Constants is
    Test,
    PolygonConstants,
    ArbitrumConstants,
    BaseConstants,
    BscConstants
{
    address ADMIN = makeAddr("unique-admin"); //admin=us
    // Changed this since someone deployed a drainer contract to makeAddr("bob")
    address BOB = makeAddr("bob*"); //bob=someone!=us
    address EXECUTOR_SETTER = makeAddr("executorSetter");
    address FEE_CALCULATOR = makeAddr("feeCalculator");
    address FEE_SETTER = makeAddr("feeSetter");
    bytes32 ROUTER_FEE_SETTER_ROLE =
        0x9939157be7760e9462f1d5a0dcad88b616ddc64138e317108b40b1cf55601348;
    address ALICE = 0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2;
    uint256 ALICE_PK =
        0x123456789abcdef123456789abcdef123456789abcdef123456789abcdef1234;

    // Client fee receiver key pair — address is vm.addr(CLIENT_FEE_RECEIVER_PK)
    uint256 CLIENT_FEE_RECEIVER_PK =
        0x6789abcdef1234567890abcdef1234567890abcdef1234567890abcdef123456;

    // Dummy contracts
    address DUMMY = makeAddr("dummy");
    address DUMMY2 = makeAddr("dummy2");
    address DUMMY3 = makeAddr("dummy3");
    address PAUSER = makeAddr("pauser");
    address UNPAUSER = makeAddr("unpauser");

    // Assets
    address ETH_ADDR = address(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE);
    address WETH_ADDR = address(0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2);
    address DAI_ADDR = address(0x6B175474E89094C44Da98b954EedeAC495271d0F);
    address BAL_ADDR = address(0xba100000625a3754423978a60c9317c58a424e3D);
    address USDC_ADDR = address(0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48);
    address WBTC_ADDR = address(0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599);
    address INCH_ADDR = address(0x111111111117dC0aa78b770fA6A738034120C302);
    address USDE_ADDR = address(0x4c9EDD5852cd905f086C759E8383e09bff1E68B3);
    address USDT_ADDR = address(0xdAC17F958D2ee523a2206206994597C13D831ec7);
    address PEPE_ADDR = address(0x6982508145454Ce325dDbE47a25d4ec3d2311933);
    address STETH_ADDR = address(0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84);
    address RETH_ADDR = address(0xae78736Cd615f374D3085123A210448E74Fc6393);
    address WSTETH_ADDR = address(0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0);
    address LUSD_ADDR = address(0x5f98805A4E8be255a32880FDeC7F6728C6568bA0);
    address LDO_ADDR = address(0x5A98FcBEA516Cf06857215779Fd812CA3beF1B32);
    address CRV_ADDR = address(0xD533a949740bb3306d119CC777fa900bA034cd52);
    address ADAI_ADDR = address(0x028171bCA77440897B824Ca71D1c56caC55b68A3);
    address AUSDC_ADDR = address(0xBcca60bB61934080951369a648Fb03DF4F96263C);
    address SUSD_ADDR = address(0x57Ab1ec28D129707052df4dF418D58a2D46d5f51);
    address FRAX_ADDR = address(0x853d955aCEf822Db058eb8505911ED77F175b99e);
    address DOLA_ADDR = address(0x865377367054516e17014CcdED1e7d814EDC9ce4);
    address XYO_ADDR = address(0x55296f69f40Ea6d20E478533C15A6B08B654E758);
    address UWU_ADDR = address(0x55C08ca52497e2f1534B59E2917BF524D4765257);
    address CRVUSD_ADDR = address(0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E);
    address WSTTAO_ADDR = address(0xe9633C52f4c8B7BDeb08c4A7fE8a5c1B84AFCf67);
    address WTAO_ADDR = address(0x77E06c9eCCf2E797fd462A92B6D7642EF85b0A44);
    address BSGG_ADDR = address(0xdA16Cf041E2780618c49Dbae5d734B89a6Bac9b3);
    address GHO_ADDR = address(0x40D16FC0246aD3160Ccc09B8D0D3A2cD28aE6C2f);
    address ONDO_ADDR = address(0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3);
    address RLUSD_ADDR = address(0x8292Bb45bf1Ee4d140127049757C2E0fF06317eD);

    // Maverick v2
    address MAVERICK_V2_FACTORY = 0x0A7e848Aca42d879EF06507Fca0E7b33A0a63c1e;
    address GHO_USDC_POOL = 0x14Cf6D2Fe3E1B326114b07d22A6F6bb59e346c67;

    // Uniswap v2
    address DAI_WETH_UNIV2_POOL = 0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11;
    address DAI_USDC_POOL = 0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5;
    address WETH_WBTC_POOL = 0xBb2b8038a1640196FbE3e38816F3e67Cba72D940;
    address USDC_WBTC_POOL = 0x004375Dff511095CC5A197A54140a24eFEF3A416;
    address USDC_WETH_USV2 = 0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc;

    // Sushiswap v2
    address SUSHISWAP_WBTC_WETH_POOL =
        0xCEfF51756c56CeFFCA006cD410B03FFC46dd3a58;

    // Pancakeswap v2
    address PANCAKESWAP_WBTC_WETH_POOL =
        0x4AB6702B3Ed3877e9b1f203f90cbEF13d663B0e8;

    // Uniswap v3
    address DAI_WETH_USV3 = 0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8;
    address DAI_USDT_USV3 = 0x48DA0965ab2d2cbf1C17C09cFB5Cbe67Ad5B1406;
    address USDC_WETH_USV3 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640; // 0.05% fee
    address USDC_WETH_USV3_2 = 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8; // 0.3% fee

    // Pancakeswap v3
    address PANCAKESWAPV3_WETH_USDT_POOL =
        0x6CA298D2983aB03Aa1dA7679389D955A4eFEE15C;

    // Factories
    address USV3_FACTORY_ETHEREUM = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    address USV2_FACTORY_ETHEREUM = 0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f;
    address SUSHISWAPV2_FACTORY_ETHEREUM =
        0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac;
    address PANCAKESWAPV2_FACTORY_ETHEREUM =
        0x1097053Fd2ea711dad45caCcc45EfF7548fCB362;

    // Pancakeswap uses their deployer instead of their factory for target verification
    address PANCAKESWAPV3_DEPLOYER_ETHEREUM =
        0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9;

    // Curve
    address TRIPOOL = 0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7;
    address TRICRYPTO_POOL = 0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B;
    address STETH_POOL = 0xDC24316b9AE028F1497c275EB9192a3Ea0f67022;
    address LUSD_POOL = 0xEd279fDD11cA84bEef15AF5D39BB4d4bEE23F0cA;
    address CPOOL = 0xA2B47E3D5c44877cca798226B7B8118F9BFb7A56;
    address LDO_POOL = 0x9409280DC1e6D33AB7A8C6EC03e5763FB61772B5;
    address CRV_POOL = 0x8301AE4fc9c624d1D396cbDAa1ed877821D7C511;
    address AAVE_POOL = 0xDeBF20617708857ebe4F679508E7b7863a8A8EeE;
    address FRAXPYUSD_POOL =
        address(0xA5588F7cdf560811710A2D82D3C9c99769DB1Dcb);
    address TRICRYPTO2_POOL = 0xD51a44d3FaE010294C616388b506AcdA1bfAAE46;
    address SUSD_POOL = 0xA5407eAE9Ba41422680e2e00537571bcC53efBfD;
    address FRAX_USDC_POOL = 0xDcEF968d416a41Cdac0ED8702fAC8128A64241A2;
    address USDE_USDC_POOL = 0x02950460E2b9529D0E00284A5fA2d7bDF3fA4d72;
    address DOLA_FRAXPYUSD_POOL = 0xef484de8C07B6e2d732A92B5F78e81B38f99f95E;
    address ETH_XYO_POOL = 0x99e09ee2d6Bb16c0F5ADDfEA649dbB2C1d524624;
    address UWU_WETH_POOL = 0x77146B0a1d08B6844376dF6d9da99bA7F1b19e71;
    address CRVUSD_USDT_POOL = 0x390f3595bCa2Df7d23783dFd126427CCeb997BF4;
    address WSTTAO_WTAO_POOL = 0xf2DCf6336D8250754B4527f57b275b19c8D5CF88;
    address BSGG_USDT_POOL = 0x5500307Bcf134E5851FB4D7D8D1Dc556dCdB84B4;

    // Uniswap universal router
    address UNIVERSAL_ROUTER = 0x66a9893cC07D91D95644AEDD05D03f95e1dBA8Af;

    // Permit2
    address PERMIT2_ADDRESS = 0x000000000022D473030F116dDEE9F6B43aC78BA3;

    // Bebop Settlement
    address BEBOP_SETTLEMENT = 0xbbbbbBB520d69a9775E85b458C58c648259FAD5F;

    // Hashflow Router
    address HASHFLOW_ROUTER = 0x55084eE0fEf03f14a305cd24286359A35D735151;

    // Pool Code Init Hashes
    bytes32 USV2_POOL_CODE_INIT_HASH =
        0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f;
    bytes32 USV3_POOL_CODE_INIT_HASH =
        0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    bytes32 SUSHIV2_POOL_CODE_INIT_HASH =
        0xe18a34eb0e04b04f7a0ac29a6e80748dca96319b42c54d679cb821dca90c6303;
    bytes32 PANCAKEV2_POOL_CODE_INIT_HASH =
        0x57224589c67f3f30a6b0d7a1b54cf3153ab84563bc609ef41dfb34f8b2974d2d;
    bytes32 PANCAKEV3_POOL_CODE_INIT_HASH =
        0x6ce8eb472fa82df5469c6ab6d485f17c3ad13c8cd7af59b3d4a8026c5ce0f7e2;

    // Curve meta registry
    address CURVE_META_REGISTRY = 0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC;

    // Uniswap v4 pool manager
    address POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;

    // Fluid liquidity contract on mainnet
    address FLUIDV1_LIQUIDITY = 0x52Aa899454998Be5b000Ad077a46Bbe360F4e497;

    // FermiSwap
    address FERMI_SWAPPER = 0xb1076fE3AB5e28005C7c323Bac5AC06a680d452e;

    // Angstrom hook address
    address ANGSTROM_HOOK = 0x0000000aa232009084Bd71A5797d089AA4Edfad4;

    // Liquorice
    address LIQUORICE_SETTLEMENT = 0x0448633eb8B0A42EfED924C42069E0DcF08fb552;
    address LIQUORICE_BALANCE_MANAGER =
        0xb87bAE43a665EB5943A5642F81B26666bC9E5C95;

    // Etherfi
    address EETH_ADDR = 0x35fA164735182de50811E8e2E824cFb9B6118ac2;
    address WEETH_ADDR = 0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee;
    address LIQUIDITY_POOL_ADDR = 0x308861A430be4cce5502d0A12724771Fc6DaF216;
    address REDEMPTION_MANAGER_ADDR =
        0xDadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0;

    // Rocketpool v1.4 (Saturn 1) deposit pool
    address ROCKET_DEPOSIT_POOL = 0xCE15294273CFb9D9b628F4D61636623decDF4fdC;

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
