// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

/// @dev external struct including a generic encoded order and swapper signature
///  The order bytes will be parsed and mapped to a ResolvedOrder in the concrete reactor contract
struct SignedOrder {
    bytes order;
    bytes sig;
}

struct OrderInfo {
    // The address of the reactor that this order is targeting
    // Note that this must be included in every order so the swapper
    // signature commits to the specific reactor that they trust to fill their order properly
    address reactor;
    // The address of the user which created the order
    // Note that this must be included so that order hashes are unique by swapper
    address swapper;
    // The nonce of the order, allowing for signature replay protection and cancellation
    uint256 nonce;
    // The timestamp after which this order is no longer valid
    uint256 deadline;
    // Custom validation contract
    address additionalValidationContract;
    // Encoded validation params for additionalValidationContract
    bytes additionalValidationData;
}

/// @dev tokens that need to be sent from the swapper in order to satisfy an order
struct InputToken {
    address token;
    uint256 amount;
    // Needed for dutch decaying inputs
    uint256 maxAmount;
}

/// @dev tokens that need to be received by the recipient in order to satisfy an order
struct OutputToken {
    address token;
    uint256 amount;
    address recipient;
}

/// @dev generic concrete order that specifies exact tokens which need to be sent and received
struct ResolvedOrder {
    OrderInfo info;
    InputToken input;
    OutputToken[] outputs;
    bytes sig;
    bytes32 hash;
}

struct DutchOutput {
    address token;
    uint256 startAmount;
    uint256 endAmount;
    address recipient;
}

struct DutchInput {
    address token;
    uint256 startAmount;
    uint256 endAmount;
}

struct ExclusiveDutchOrder {
    OrderInfo info;
    uint256 decayStartTime;
    uint256 decayEndTime;
    address exclusiveFiller;
    uint256 exclusivityOverrideBps;
    DutchInput input;
    DutchOutput[] outputs;
}

struct DutchOrder {
    OrderInfo info;
    uint256 decayStartTime;
    uint256 decayEndTime;
    address exclusiveFiller;
    uint256 exclusivityOverrideBps;
    DutchInput input;
    DutchOutput[] outputs;
}

struct CosignerData {
    // The time at which the DutchOutputs start decaying
    uint256 decayStartTime;
    // The time at which price becomes static
    uint256 decayEndTime;
    // The address who has exclusive rights to the order until decayStartTime
    address exclusiveFiller;
    // The amount in bps that a non-exclusive filler needs to improve the outputs by to be able to fill the order
    uint256 exclusivityOverrideBps;
    // The tokens that the swapper will provide when settling the order
    uint256 inputAmount;
    // The tokens that must be received to satisfy the order
    uint256[] outputAmounts;
}

struct V2DutchOrder {
    // generic order information
    OrderInfo info;
    // The address which must cosign the full order
    address cosigner;
    // The tokens that the swapper will provide when settling the order
    DutchInput baseInput;
    // The tokens that must be received to satisfy the order
    DutchOutput[] baseOutputs;
    // signed over by the cosigner
    CosignerData cosignerData;
    // signature from the cosigner over (orderHash || cosignerData)
    bytes cosignature;
}
