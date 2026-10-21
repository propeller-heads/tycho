// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {Pausable} from "@openzeppelin/contracts/utils/Pausable.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {ISubsidyPayer, Subsidy} from "@interfaces/ISubsidyPayer.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";

error SubsidyPayer__NotRouter();
error SubsidyPayer__AddressZero();

/**
 * @title SubsidyPayer
 * @notice Holds the subsidy budget and pays one signed subsidy per nonce.
 * @dev Only TychoRouterV3 may call `pay`, and `pay` never reverts for it.
 *      Each skip emits `SubsidySkipped` and returns false, so the swap fills
 *      as it would without the subsidy.
 *
 *      No address holds `DEFAULT_ADMIN_ROLE`. Every role administers itself
 *      except `PAUSER_ROLE`, which `PAUSER_ADMIN_ROLE` administers. Pauser keys
 *      are hot, so a leaked one must not be able to revoke the other pausers.
 */
contract SubsidyPayer is ISubsidyPayer, AccessControl, Pausable, EIP712 {
    using SafeERC20 for IERC20;

    struct Roles {
        address signerSetter;
        address budgetManager;
        address pauserAdmin;
        address unpauser;
        address[] pausers;
    }

    // Gates `setSigner` and `invalidateNonces`
    bytes32 public constant SIGNER_SETTER_ROLE =
        keccak256("SIGNER_SETTER_ROLE");
    // Gates `withdraw`
    bytes32 public constant BUDGET_ROLE = keccak256("BUDGET_ROLE");
    // Gates `unpause`
    bytes32 public constant UNPAUSER_ROLE = keccak256("UNPAUSER_ROLE");
    // Grants and revokes `PAUSER_ROLE`
    bytes32 public constant PAUSER_ADMIN_ROLE = keccak256("PAUSER_ADMIN_ROLE");
    // Gates `pause`
    bytes32 public constant PAUSER_ROLE = keccak256("PAUSER_ROLE");

    bytes32 public constant SUBSIDY_TYPEHASH = keccak256(
        "Subsidy(address boundTo,address token,uint256 amount,"
        "uint256 minAmountIn,bytes32 innerSwapHash,uint256 nonce,"
        "uint256 deadline)"
    );

    // Gas forwarded with a native ETH subsidy, on top of the 2300 the EVM
    // adds to a call with value. On a mainnet fork with cold state,
    // receiving ETH costs a Safe (1.3.0 and 1.4.1, plain and L2) 4029 gas and
    // a Coinbase Smart Wallet 2507. The cap stops a gas-burning receiver from
    // running the swap out of gas.
    uint256 private constant _ETH_TRANSFER_GAS = 20_000;

    address public immutable router;
    address public signer;

    // Unordered nonces: word `nonce >> 8`, bit `nonce & 0xff`
    mapping(uint256 => uint256) private _nonceBitmap;

    event SignerUpdated(address indexed oldSigner, address indexed newSigner);
    event NoncesInvalidated(uint256 indexed word, uint256 mask);
    event Withdrawal(
        address indexed token, uint256 amount, address indexed receiver
    );

    /**
     * @param router_ The only address allowed to call `pay`
     * @param signer_ The initial `signer`, whose signatures `pay` accepts
     * @param roles Initial holder of each role
     */
    constructor(address router_, address signer_, Roles memory roles)
        EIP712("TychoSubsidyPayer", "1")
    {
        if (
            router_ == address(0) || signer_ == address(0)
                || roles.signerSetter == address(0)
                || roles.budgetManager == address(0)
                || roles.pauserAdmin == address(0)
                || roles.unpauser == address(0)
        ) {
            revert SubsidyPayer__AddressZero();
        }
        router = router_;
        signer = signer_;
        emit SignerUpdated(address(0), signer_);

        _setRoleAdmin(SIGNER_SETTER_ROLE, SIGNER_SETTER_ROLE);
        _setRoleAdmin(BUDGET_ROLE, BUDGET_ROLE);
        _setRoleAdmin(UNPAUSER_ROLE, UNPAUSER_ROLE);
        _setRoleAdmin(PAUSER_ADMIN_ROLE, PAUSER_ADMIN_ROLE);
        _setRoleAdmin(PAUSER_ROLE, PAUSER_ADMIN_ROLE);

        _grantRole(SIGNER_SETTER_ROLE, roles.signerSetter);
        _grantRole(BUDGET_ROLE, roles.budgetManager);
        _grantRole(UNPAUSER_ROLE, roles.unpauser);
        _grantRole(PAUSER_ADMIN_ROLE, roles.pauserAdmin);
        for (uint256 i = 0; i < roles.pausers.length; i++) {
            if (roles.pausers[i] == address(0)) {
                revert SubsidyPayer__AddressZero();
            }
            _grantRole(PAUSER_ROLE, roles.pausers[i]);
        }
    }

    /// @notice Funds the native ETH budget.
    receive() external payable {}

    /// @inheritdoc ISubsidyPayer
    function pay(
        Subsidy calldata subsidy,
        uint256 hopAmountIn,
        address routerMsgSender,
        address to,
        bytes calldata signature
    ) external returns (bool paid) {
        if (msg.sender != router) revert SubsidyPayer__NotRouter();

        uint256 nonce = subsidy.nonce;
        SkipReason reason =
            _skipReason(subsidy, hopAmountIn, routerMsgSender, signature);
        if (reason != SkipReason.None) {
            emit SubsidySkipped(nonce, reason);
            return false;
        }

        // Mark the nonce used before the transfer, so a receiver that
        // re-enters the router cannot claim it twice
        (uint256 word, uint256 bit) = _nonceBit(nonce);
        _nonceBitmap[word] |= bit;
        if (!_transfer(subsidy.token, to, subsidy.amount)) {
            _nonceBitmap[word] &= ~bit;
            emit SubsidySkipped(nonce, SkipReason.TransferFailed);
            return false;
        }
        // Emitted after the transfer on purpose: only then is it paid. A
        // re-entering receiver cannot pay twice, because the nonce is
        // marked first and only the nonReentrant router can call `pay`.
        // slither-disable-next-line reentrancy-events
        emit SubsidyPaid(
            nonce, subsidy.boundTo, to, subsidy.token, subsidy.amount
        );
        return true;
    }

    /**
     * @notice Replaces `signer`. Reverts on the zero address; pause the
     *         contract to stop subsidies.
     */
    function setSigner(address newSigner)
        external
        onlyRole(SIGNER_SETTER_ROLE)
    {
        if (newSigner == address(0)) {
            revert SubsidyPayer__AddressZero();
        }
        emit SignerUpdated(signer, newSigner);
        signer = newSigner;
    }

    /// @notice Sends `amount` of `token` from the budget to `to`. Use
    ///         ETH_ADDRESS for native ETH.
    function withdraw(address token, address to, uint256 amount)
        external
        onlyRole(BUDGET_ROLE)
    {
        if (to == address(0)) revert SubsidyPayer__AddressZero();
        emit Withdrawal(token, amount, to);
        if (token == ETH_ADDRESS) {
            Address.sendValue(payable(to), amount);
        } else {
            IERC20(token).safeTransfer(to, amount);
        }
    }

    /**
     * @notice Marks nonces used, cancelling signatures that leaked but have
     *         not been paid yet.
     * @param word The bitmap word, `nonce >> 8`
     * @param mask The bits to set, `1 << (nonce & 0xff)` for each nonce
     */
    function invalidateNonces(uint256 word, uint256 mask)
        external
        onlyRole(SIGNER_SETTER_ROLE)
    {
        _nonceBitmap[word] |= mask;
        emit NoncesInvalidated(word, mask);
    }

    function pause() external onlyRole(PAUSER_ROLE) {
        _pause();
    }

    function unpause() external onlyRole(UNPAUSER_ROLE) {
        _unpause();
    }

    function isNonceUsed(uint256 nonce) public view returns (bool) {
        (uint256 word, uint256 bit) = _nonceBit(nonce);
        return _nonceBitmap[word] & bit != 0;
    }

    function _nonceBit(uint256 nonce)
        private
        pure
        returns (uint256 word, uint256 bit)
    {
        word = nonce >> 8;
        // forge-lint: disable-next-line(incorrect-shift)
        bit = 1 << (nonce & 0xff);
    }

    /**
     * @dev Returns the `SkipReason` of the first check `subsidy` fails, or
     *      `None` when it can be paid.
     */
    function _skipReason(
        Subsidy calldata subsidy,
        uint256 hopAmountIn,
        address routerMsgSender,
        bytes calldata signature
    ) private view returns (SkipReason) {
        // Checks `paused()` instead of using `whenNotPaused`, which reverts:
        // a pause must only skip the subsidy, so swaps still fill without it
        // and pausing never disrupts swap flow
        if (paused()) {
            return SkipReason.Paused;
        }
        // A few seconds of validator drift on the deadline is harmless
        // slither-disable-next-line timestamp
        if (block.timestamp > subsidy.deadline) {
            return SkipReason.Expired;
        }
        if (!_isSignedBySigner(subsidy, signature)) {
            return SkipReason.InvalidSignature;
        }
        // Pays only when the quoted sender (`boundTo`) is sending this swap:
        // - `routerMsgSender` is whoever called the router, not the router
        //   itself. The subsidy executor runs by delegatecall, so it passes
        //   the router's own msg.sender, and only the router may call `pay`:
        //   no caller can set it. It covers EOAs, smart wallets and
        //   integrator contracts that call the router directly.
        // - `tx.origin` covers a user EOA that calls the router through an
        //   integrator contract, so the router's caller is the integrator.
        // A copied signature sent by anyone else matches neither and is
        // skipped without using the nonce. tx.origin only decides who earns
        // the subsidy; it guards no funds.
        // slither-disable-next-line tx-origin
        if (subsidy.boundTo != routerMsgSender && subsidy.boundTo != tx.origin)
        {
            return SkipReason.CallerMismatch;
        }
        if (hopAmountIn < subsidy.minAmountIn) {
            return SkipReason.BelowMinAmountIn;
        }
        if (isNonceUsed(subsidy.nonce)) return SkipReason.NonceUsed;
        if (_balanceOf(subsidy.token) < subsidy.amount) {
            return SkipReason.InsufficientBalance;
        }
        return SkipReason.None;
    }

    function _isSignedBySigner(
        Subsidy calldata subsidy,
        bytes calldata signature
    ) private view returns (bool) {
        // Every Subsidy field is static, so `abi.encode` lays them out in
        // order, as EIP-712 `encodeData` requires
        bytes32 structHash = keccak256(abi.encode(SUBSIDY_TYPEHASH, subsidy));
        // The ignored third return value only adds detail to `err`; `err`
        // alone decides the rejection
        // slither-disable-next-line unused-return
        (address recovered, ECDSA.RecoverError err,) =
            ECDSA.tryRecoverCalldata(_hashTypedDataV4(structHash), signature);
        return err == ECDSA.RecoverError.NoError && recovered == signer;
    }

    /// @dev Returns zero when `token` has no code, or its `balanceOf` reverts
    ///      or returns fewer than 32 bytes, so `pay` skips instead of
    ///      reverting.
    function _balanceOf(address token)
        private
        view
        returns (uint256 tokenBalance)
    {
        if (token == ETH_ADDRESS) return address(this).balance;
        bytes4 selector = IERC20.balanceOf.selector;
        bool success;
        uint256 returnSize;
        // Assembly, because a Solidity call copies all return data into
        // memory, and memory gas grows quadratically. Copying at most 32
        // bytes means a token returning a huge payload cannot make `pay`
        // spend its gas on that copy.
        // slither-disable-next-line assembly
        assembly ("memory-safe") {
            let ptr := mload(0x40)
            mstore(ptr, selector)
            mstore(add(ptr, 0x04), address())
            success := staticcall(gas(), token, ptr, 0x24, 0x00, 0x20)
            returnSize := returndatasize()
            tokenBalance := mload(0x00)
        }
        if (!success || returnSize < 32) return 0;
    }

    /// @dev Sends `amount` of `token` to `to`; returns false instead of
    ///      reverting on failure.
    function _transfer(address token, address to, uint256 amount)
        private
        returns (bool success)
    {
        if (token != ETH_ADDRESS) {
            return IERC20(token).trySafeTransfer(to, amount);
        }
        // The gas cap means a receiver that reverts, burns gas or returns a
        // huge payload only loses its subsidy: it cannot spend more than the
        // cap, which also bounds the return data copied back
        // (testPayEthFailingReceiver measures that copy).
        // slither-disable-next-line low-level-calls,arbitrary-send-eth,return-bomb
        (success,) = to.call{value: amount, gas: _ETH_TRANSFER_GAS}("");
    }
}
