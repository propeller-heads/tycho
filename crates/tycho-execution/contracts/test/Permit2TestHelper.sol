pragma solidity ^0.8.26;

import "./Constants.sol";
import "@permit2/src/interfaces/IAllowanceTransfer.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

contract Permit2TestHelper is Constants {
    /**
     * @dev Handles the Permit2 approval process for Alice, allowing the TychoRouterV3 contract
     *      to spend `amount_in` of `tokenIn` on her behalf.
     *
     * This function approves the Permit2 contract to transfer the specified token amount
     * and constructs a `PermitSingle` struct for the approval. It also generates a valid
     * EIP-712 signature for the approval using Alice's private key.
     *
     * @param tokenIn The address of the token being approved.
     * @param amountIn The amount of tokens to approve for transfer.
     * @return permitSingle The `PermitSingle` struct containing the approval details.
     * @return signature The EIP-712 signature for the approval.
     */
    function handlePermit2Approval(
        address tokenIn,
        address spender,
        uint256 amountIn
    ) internal returns (IAllowanceTransfer.PermitSingle memory, bytes memory) {
        IERC20(tokenIn).approve(PERMIT2_ADDRESS, amountIn);
        IAllowanceTransfer.PermitSingle memory permitSingle =
            IAllowanceTransfer.PermitSingle({
                details: IAllowanceTransfer.PermitDetails({
                    token: tokenIn,
                    amount: uint160(amountIn),
                    expiration: uint48(block.timestamp + 1 days),
                    nonce: 0
                }),
                spender: spender,
                sigDeadline: block.timestamp + 1 days
            });

        bytes memory signature = signPermit2(permitSingle, ALICE_PK);
        return (permitSingle, signature);
    }

    /**
     * @dev Signs a Permit2 `PermitSingle` struct with the given private key.
     * @param permit The `PermitSingle` struct to sign.
     * @param privateKey The private key of the signer.
     * @return The signature as a `bytes` array.
     */
    function signPermit2(
        IAllowanceTransfer.PermitSingle memory permit,
        uint256 privateKey
    ) internal view returns (bytes memory) {
        bytes32 _permitDetailsTypehash = keccak256(
            "PermitDetails(address token,uint160 amount,uint48 expiration,uint48 nonce)"
        );
        bytes32 _permitSingleTypehash = keccak256(
            "PermitSingle(PermitDetails details,address spender,uint256 sigDeadline)PermitDetails(address token,uint160 amount,uint48 expiration,uint48 nonce)"
        );
        bytes32 domainSeparator = keccak256(
            abi.encode(
                keccak256(
                    "EIP712Domain(string name,uint256 chainId,address verifyingContract)"
                ),
                keccak256("Permit2"),
                block.chainid,
                PERMIT2_ADDRESS
            )
        );
        bytes32 detailsHash =
            keccak256(abi.encode(_permitDetailsTypehash, permit.details));
        bytes32 permitHash = keccak256(
            abi.encode(
                _permitSingleTypehash,
                detailsHash,
                permit.spender,
                permit.sigDeadline
            )
        );

        bytes32 digest = keccak256(
            abi.encodePacked("\x19\x01", domainSeparator, permitHash)
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(privateKey, digest);

        return abi.encodePacked(r, s, v);
    }
}
