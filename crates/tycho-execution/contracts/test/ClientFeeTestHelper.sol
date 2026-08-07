pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {ClientFeeParams} from "@src/TychoRouterV3.sol";
import {Constants} from "./Constants.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {IERC1271} from "@openzeppelin/contracts/interfaces/IERC1271.sol";

/**
 * @dev Minimal ERC-1271 smart contract wallet. Validates a digest by
 *      recovering the accompanying ECDSA signature and comparing it against a
 *      single owner, the way a multisig validates an owner's signature.
 */
contract ERC1271Wallet is IERC1271 {
    address private immutable _owner;

    constructor(address owner) {
        _owner = owner;
    }

    function isValidSignature(bytes32 hash, bytes calldata signature)
        external
        view
        returns (bytes4)
    {
        (address recovered, ECDSA.RecoverError err,) =
            ECDSA.tryRecover(hash, signature);
        if (err == ECDSA.RecoverError.NoError && recovered == _owner) {
            return IERC1271.isValidSignature.selector;
        }
        return 0xffffffff;
    }
}

/**
 * @dev Contract without an ERC-1271 implementation. A staticcall to
 *      isValidSignature reverts, so signature verification must fail.
 */
contract NonERC1271Wallet {}

contract ClientFeeTestHelper is Test, Constants {
    bytes32 private constant _CLIENT_FEE_TYPEHASH = keccak256(
        "ClientFee(uint32 clientFeeBps,address clientFeeReceiver,"
        "uint256 maxClientContribution,uint256 deadline,"
        "uint256 amountIn,address tokenIn,address tokenOut,"
        "uint256 expectedAmountOut,uint256 minAmountOut,address receiver,bytes swaps)"
    );

    /**
     * @dev Signs a ClientFeeParams struct with the given private key,
     *      producing the EIP-712 signature expected by TychoRouterV3.
     */
    function signClientFee(
        ClientFeeParams memory params,
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        bytes memory swapData,
        address routerAddress,
        uint256 privateKey
    ) internal view returns (bytes memory signature) {
        return signClientFeeForChain(
            params,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            swapData,
            routerAddress,
            block.chainid,
            privateKey
        );
    }

    /**
     * @dev Signs a ClientFeeParams struct for a specific chain ID.
     *      Used to test that signatures from a different chain are rejected.
     */
    function signClientFeeForChain(
        ClientFeeParams memory params,
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        bytes memory swapData,
        address routerAddress,
        uint256 chainId,
        uint256 privateKey
    ) internal view returns (bytes memory signature) {
        bytes32 domainSeparator = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,string version,"
                    "uint256 chainId,address verifyingContract)"
                ),
                keccak256("TychoRouter"),
                keccak256("1"),
                chainId,
                routerAddress
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(
                _CLIENT_FEE_TYPEHASH,
                params.clientFeeBps,
                params.clientFeeReceiver,
                params.maxClientContribution,
                params.deadline,
                amountIn,
                tokenIn,
                tokenOut,
                expectedAmountOut,
                minAmountOut,
                receiver,
                keccak256(swapData)
            )
        );
        bytes32 digest = keccak256(
            abi.encodePacked("\x19\x01", domainSeparator, structHash)
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(privateKey, digest);
        return abi.encodePacked(r, s, v);
    }

    /**
     * @dev Returns an empty ClientFeeParams for calls that do not use client fees.
     */
    function noClientFee()
        internal
        pure
        returns (ClientFeeParams memory params)
    {
        params = ClientFeeParams({
            clientFeeBps: 0,
            clientFeeReceiver: address(0),
            maxClientContribution: 0,
            deadline: 0,
            clientSignature: new bytes(0)
        });
    }

    /**
     * @dev Builds and signs a ClientFeeParams struct using the given private key.
     *      The signer address is derived from the private key and used as clientFeeReceiver.
     */
    function makeClientFeeParams(
        uint32 clientFeeBps,
        uint256 maxClientContribution,
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        bytes memory swapData,
        address routerAddress,
        uint256 privateKey
    ) internal view returns (ClientFeeParams memory params) {
        address feeReceiver = vm.addr(privateKey);
        params = ClientFeeParams({
            clientFeeBps: clientFeeBps,
            clientFeeReceiver: feeReceiver,
            maxClientContribution: maxClientContribution,
            deadline: block.timestamp + 1 hours,
            clientSignature: new bytes(0)
        });
        params.clientSignature = signClientFee(
            params,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            swapData,
            routerAddress,
            privateKey
        );
    }
}
