# pragma version 0.4.3
"""
@title StableswapMath
@author Curve.Fi
@license Copyright (c) Curve.Fi, 2020 - 2025 - all rights reserved
@notice StableSwapNG adapted for use in twocrypto pool.
"""

from snekmate.utils import math

N_COINS: constant(uint256) = 2
A_MULTIPLIER: constant(uint256) = 10000

version: public(constant(String[8])) = "v0.1.1"

# ------------------------ AMM math functions --------------------------------

@external
@pure
def get_y(
    _amp: uint256,
    _gamma: uint256, # unused, present for compatibility with twocrypto
    xp: uint256[N_COINS],
    D: uint256,
    i: uint256
) -> uint256[2]: # returns [y, 0] (0 is unused, present for compatibility with twocrypto)
    """
    Calculate xp[i] for given xp[j] (j != i) and D.
    """
    # xp in the input is converted to the same price/precision

    assert i < N_COINS  # dev: i above N_COINS

    S_: uint256 = 0
    _x: uint256 = 0
    y_prev: uint256 = 0
    c: uint256 = D
    Ann: uint256 = _amp * N_COINS

    for _i: uint256 in range(N_COINS):
        if _i != i:
            _x = xp[_i]
        else:
            continue
        S_ += _x
        c = c * D // (_x * N_COINS)

    c = c * D * A_MULTIPLIER // (Ann * N_COINS)
    b: uint256 = S_ + D * A_MULTIPLIER // Ann
    y: uint256 = D

    for _i: uint256 in range(255):
        y_prev = y
        y = (y*y + c) // (2 * y + b - D)
        # Equality with the precision of 1
        if y > y_prev:
            if y - y_prev <= 1:
                return [y, 0]
        else:
            if y_prev - y <= 1:
                return [y, 0]
    raise "Did not converge"


@external
@pure
def newton_D(_amp: uint256,
    gamma: uint256, # unused, present for compatibility with twocrypto
    _xp: uint256[N_COINS],
    K0_prev: uint256 = 0 # unused, present for compatibility with twocrypto
) -> uint256:
    """
    Find D for given x[i] and A.
    """
    # gamma and K0_prev are ignored
    # _amp is already multiplied by a A_MULTIPLIER and N_COINS
    assert (
        _xp[0] > 0 and
        _xp[1] > 0 and
        unsafe_div(max(_xp[0], _xp[1]), min(_xp[0], _xp[1])) < 10_000
    ), "!balance"

    S: uint256 = 0
    for x: uint256 in _xp:
        S += x
    if S == 0:
        return 0

    D: uint256 = S
    Ann: uint256 = _amp * N_COINS

    for i: uint256 in range(255):

        D_P: uint256 = D
        for x: uint256 in _xp:
            D_P = D_P * D // x
        D_P //= N_COINS**N_COINS
        Dprev: uint256 = D

        # (Ann * S / A_PRECISION + D_P * N_COINS) * D / ((Ann - A_PRECISION) * D / A_PRECISION + (N_COINS + 1) * D_P)
        D = (
            (unsafe_div(Ann * S, A_MULTIPLIER) + D_P * N_COINS) * D
            //
            (
                unsafe_div((Ann - A_MULTIPLIER) * D, A_MULTIPLIER) +
                unsafe_add(N_COINS, 1) * D_P
            )
        )

        # Equality with the precision of 1
        if D > Dprev:
            if D - Dprev <= 1:
                return D
        else:
            if Dprev - D <= 1:
                return D
    # convergence typically occurs in 4 rounds or less, this should be unreachable!
    # if it does happen the pool is borked and LPs can withdraw via `remove_liquidity`
    raise "Did not converge"


@external
@pure
def get_p(
    _xp: uint256[N_COINS], _D: uint256, _A_gamma: uint256[N_COINS]
) -> uint256:
    """
    @notice Calculates dx/dy.
    @dev Output needs to be multiplied with price_scale to get the actual value.
    @param _xp Balances of the pool.
    @param _D Current value of D.
    @param _A_gamma Amplification coefficient and gamma.
    """
    # dx_0 / dx_1 only, however can have any number of coins in pool
    ANN: uint256 = unsafe_mul(_A_gamma[0], N_COINS)
    Dr: uint256 = unsafe_div(_D, N_COINS**N_COINS)

    for i: uint256 in range(N_COINS):
        Dr = Dr * _D // _xp[i]

    xp0_A: uint256 = unsafe_div(ANN * _xp[0], A_MULTIPLIER)

    return 10**18 * (xp0_A + unsafe_div(Dr * _xp[0], _xp[1])) // (xp0_A + Dr)


@external
@pure
def wad_exp(x: int256) -> uint256:
    # Explicitly cast the result to uint256 as wad_exp always returns positive values
    # This avoids implicit cast confusion between int256 and uint256
    return convert(math._wad_exp(x), uint256)
