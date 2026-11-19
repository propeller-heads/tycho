// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {ICurveFyndPolicy} from "@interfaces/ICurvePool.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";

interface ITwocryptoPool {
    function balances(uint256 i) external view returns (uint256);
    function precisions() external view returns (uint256[2] memory);
    function price_scale() external view returns (uint256);
}

/// @notice Test port of Curve's Vyper `FyndPolicy` (twocrypto-ng, branch
/// fynd-policy, 12 Nov 2026): the grant, fee and pool callback paths, with
/// its revert strings. Governance and the kill switch are left out. Remove
/// this port once the pilot pool is on chain and the tests fork it.
// forge-lint: disable-next-item(mixed-case-function,mixed-case-variable) Vyper names.
contract CurveFyndPolicy is EIP712 {
    struct ArmedGrant {
        uint256 coinIn;
        uint256 minAmountIn;
        uint256 maxAmountIn;
        uint256 fee;
        uint256[2] xp;
        uint256 priceScale;
    }

    uint256 private constant _PRECISION = 1e18;
    uint256 private constant _MAX_GRANT_WINDOW = 120;
    bytes32 public constant GRANT_TYPEHASH = keccak256(
        "Grant(address taker,uint8 coin_in,uint256 min_amount_in,uint256 max_amount_in,uint256 fee,uint64 nonce,uint32 deadline)"
    );

    address public immutable POOL;
    uint256 private immutable _PRECISION0;
    uint256 private immutable _PRECISION1;
    address public controller;
    address public router;
    uint256 public default_fee;
    uint256 public min_fee;
    mapping(uint256 => uint256) private _usedNonces;

    bool private transient _armed;
    ArmedGrant private _armedGrant;

    constructor(
        address pool,
        address controller_,
        address router_,
        uint256 defaultFee,
        uint256 minFee
    ) EIP712("FyndPolicy", "1") {
        POOL = pool;
        uint256[2] memory precisions = ITwocryptoPool(pool).precisions();
        _PRECISION0 = precisions[0];
        _PRECISION1 = precisions[1];
        controller = controller_;
        router = router_;
        default_fee = defaultFee;
        min_fee = minFee;
    }

    function submit_grant(
        ICurveFyndPolicy.Grant calldata grant,
        bytes calldata signature,
        address taker
    ) external {
        require(msg.sender == router, "router");
        require(!_armed, "armed");
        require(taker == grant.taker, "taker");
        require(block.timestamp <= grant.deadline, "expired");
        require(
            grant.deadline <= block.timestamp + _MAX_GRANT_WINDOW, "deadline"
        );
        require(grant.coin_in < 2, "coin");
        _useNonce(grant.nonce);

        bytes32 digest =
            _hashTypedDataV4(keccak256(abi.encode(GRANT_TYPEHASH, grant)));
        require(ECDSA.recover(digest, signature) == controller, "signature");

        ITwocryptoPool pool = ITwocryptoPool(POOL);
        uint256 priceScale = pool.price_scale();
        _armed = true;
        _armedGrant = ArmedGrant({
            coinIn: grant.coin_in,
            minAmountIn: grant.min_amount_in,
            maxAmountIn: grant.max_amount_in,
            fee: grant.fee,
            xp: [
                pool.balances(0) * _PRECISION0,
                pool.balances(1) * _PRECISION1 * priceScale / _PRECISION
            ],
            priceScale: priceScale
        });
    }

    function is_nonce_used(uint64 nonce) external view returns (bool) {
        return _usedNonces[nonce >> 8] & (1 << (nonce & 255)) != 0;
    }

    function get_fee(uint256[2] calldata xp) external view returns (uint256) {
        if (!_armed) return default_fee;

        ArmedGrant memory grant = _armedGrant;
        uint256[2] memory oldXp = grant.xp;
        if (xp[0] >= oldXp[0] && xp[1] >= oldXp[1]) return default_fee;
        if (xp[0] <= oldXp[0] && xp[1] <= oldXp[1]) return default_fee;

        uint256 coinIn = xp[0] > oldXp[0] ? 0 : 1;
        require(coinIn == grant.coinIn, "direction");
        uint256 amountIn = xp[coinIn] - oldXp[coinIn];
        if (coinIn == 1) amountIn = amountIn * _PRECISION / grant.priceScale;
        amountIn /= coinIn == 0 ? _PRECISION0 : _PRECISION1;
        require(
            amountIn >= grant.minAmountIn && amountIn <= grant.maxAmountIn,
            "amount"
        );
        return grant.fee > min_fee ? grant.fee : min_fee;
    }

    function get_price_scale() external pure returns (uint256) {
        return 0;
    }

    function update_pool_state(
        uint256[2] calldata,
        uint256,
        uint256,
        uint256,
        uint256,
        uint256,
        uint256,
        uint256
    ) external {
        require(msg.sender == POOL, "pool");
        _armed = false;
    }

    function _useNonce(uint64 nonce) private {
        uint256 word = nonce >> 8;
        uint256 bit = 1 << (nonce & 255);
        uint256 bitmap = _usedNonces[word];
        require(bitmap & bit == 0, "nonce");
        _usedNonces[word] = bitmap | bit;
    }

    function domainSeparator() external view returns (bytes32) {
        return _domainSeparatorV4();
    }
}
