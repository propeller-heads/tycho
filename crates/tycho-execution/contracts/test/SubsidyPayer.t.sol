// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {
    IAccessControl
} from "@openzeppelin/contracts/access/IAccessControl.sol";
import {ERC20Mock} from "@openzeppelin/contracts/mocks/token/ERC20Mock.sol";
import {
    SubsidyPayer,
    SubsidyPayer__AddressZero,
    SubsidyPayer__NotRouter
} from "@src/SubsidyPayer.sol";
import {ISubsidyPayer, Subsidy} from "@interfaces/ISubsidyPayer.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";

contract FalseReturningToken is ERC20Mock {
    function transfer(address, uint256) public pure override returns (bool) {
        return false;
    }
}

contract RevertingToken is ERC20Mock {
    function transfer(address, uint256) public pure override returns (bool) {
        revert("no transfers");
    }
}

contract RevertingBalanceToken is ERC20Mock {
    function balanceOf(address) public pure override returns (uint256) {
        revert("no balances");
    }
}

contract RevertingReceiver {
    receive() external payable {
        revert("no ETH");
    }
}

contract GasBurningReceiver {
    receive() external payable {
        while (true) {}
    }
}

contract HugeRevertReceiver {
    // Reverts with 80,000 bytes, about the most memory a receiver can expand
    // with 22,300 gas
    receive() external payable {
        assembly {
            revert(0, 80000)
        }
    }
}

/// @dev Receives ETH the way a Safe proxy does: a cold storage read and an
///      event, more than the bare 2300 stipend covers.
contract SafeLikeReceiver {
    mapping(uint256 => uint256) private _unwritten;

    event Received(uint256 value, uint256 amount);

    receive() external payable {
        emit Received(_unwritten[block.number], msg.value);
    }
}

/// @dev Reports whether the payer had marked the nonce used when ETH arrived.
contract NonceCheckingReceiver {
    SubsidyPayer private immutable _payer;
    uint256 private immutable _nonce;

    event NonceSeen(bool used);

    constructor(SubsidyPayer payer, uint256 nonce) {
        _payer = payer;
        _nonce = nonce;
    }

    receive() external payable {
        emit NonceSeen(_payer.isNonceUsed(_nonce));
    }
}

contract SubsidyPayerTest is Test {
    SubsidyPayer payer;
    ERC20Mock token;

    address ROUTER = makeAddr("router");
    address CALLER = makeAddr("caller");
    address ORIGIN = makeAddr("origin");
    address TO = makeAddr("to");
    address SIGNER_SETTER = makeAddr("signerSetter");
    address BUDGET_MANAGER = makeAddr("budgetManager");
    address PAUSER_ADMIN = makeAddr("pauserAdmin");
    address UNPAUSER = makeAddr("unpauser");
    address PAUSER_1 = makeAddr("pauser1");
    address PAUSER_2 = makeAddr("pauser2");
    address SIGNER;
    uint256 SIGNER_PK;

    uint256 constant BUDGET = 1000 ether;
    uint256 constant AMOUNT = 10 ether;
    uint256 constant MIN_AMOUNT_IN = 1 ether;

    bytes32 constant DOMAIN_TYPEHASH = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,"
        "address verifyingContract)"
    );
    bytes32 constant SUBSIDY_TYPEHASH = keccak256(
        "Subsidy(address boundTo,address token,uint256 amount,"
        "uint256 minAmountIn,bytes32 innerSwapHash,uint256 nonce,"
        "uint256 deadline)"
    );

    function setUp() public {
        (SIGNER, SIGNER_PK) = makeAddrAndKey("signer");
        payer = new SubsidyPayer(ROUTER, SIGNER, _roles());
        token = new ERC20Mock();
        token.mint(address(payer), BUDGET);
        vm.deal(address(payer), BUDGET);
    }

    function _roles() internal view returns (SubsidyPayer.Roles memory) {
        address[] memory pausers = new address[](2);
        pausers[0] = PAUSER_1;
        pausers[1] = PAUSER_2;
        return SubsidyPayer.Roles({
            signerSetter: SIGNER_SETTER,
            budgetManager: BUDGET_MANAGER,
            pauserAdmin: PAUSER_ADMIN,
            unpauser: UNPAUSER,
            pausers: pausers
        });
    }

    function _subsidy(uint256 nonce) internal view returns (Subsidy memory) {
        return Subsidy({
            boundTo: CALLER,
            token: address(token),
            amount: AMOUNT,
            minAmountIn: MIN_AMOUNT_IN,
            innerSwapHash: keccak256("inner swap"),
            nonce: nonce,
            deadline: block.timestamp + 1 hours
        });
    }

    function _sign(Subsidy memory s, uint256 pk)
        internal
        view
        returns (bytes memory)
    {
        bytes32 domainSeparator = keccak256(
            abi.encode(
                DOMAIN_TYPEHASH,
                keccak256("TychoSubsidyPayer"),
                keccak256("1"),
                block.chainid,
                address(payer)
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(
                SUBSIDY_TYPEHASH,
                s.boundTo,
                s.token,
                s.amount,
                s.minAmountIn,
                s.innerSwapHash,
                s.nonce,
                s.deadline
            )
        );
        bytes32 digest = keccak256(
            abi.encodePacked("\x19\x01", domainSeparator, structHash)
        );
        (uint8 v, bytes32 r, bytes32 sigS) = vm.sign(pk, digest);
        return abi.encodePacked(r, sigS, v);
    }

    function _sign(Subsidy memory s) internal view returns (bytes memory) {
        return _sign(s, SIGNER_PK);
    }

    /// @dev Calls `pay` as ROUTER, with `tx.origin` ORIGIN and `routerMsgSender`
    ///      CALLER.
    function _pay(Subsidy memory s, bytes memory sig, address to)
        internal
        returns (bool)
    {
        vm.prank(ROUTER, ORIGIN);
        return payer.pay(s, MIN_AMOUNT_IN, CALLER, to, sig);
    }

    function _pay(Subsidy memory s, bytes memory sig) internal returns (bool) {
        return _pay(s, sig, TO);
    }

    function _expectSkip(uint256 nonce, ISubsidyPayer.SkipReason reason)
        internal
    {
        vm.expectEmit(address(payer));
        emit ISubsidyPayer.SubsidySkipped(nonce, reason);
    }

    function _assertSkipped(
        Subsidy memory s,
        bytes memory sig,
        ISubsidyPayer.SkipReason reason
    ) internal {
        uint256 balanceBefore = _balance(s.token, TO);
        _expectSkip(s.nonce, reason);
        assertFalse(_pay(s, sig));
        assertEq(_balance(s.token, TO), balanceBefore);
        assertFalse(payer.isNonceUsed(s.nonce));
    }

    function _balance(address token_, address account)
        internal
        view
        returns (uint256)
    {
        if (token_ == ETH_ADDRESS) return account.balance;
        return ERC20Mock(token_).balanceOf(account);
    }

    function _unauthorized(address account, bytes32 role)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodeWithSelector(
            IAccessControl.AccessControlUnauthorizedAccount.selector,
            account,
            role
        );
    }

    function testPay() public {
        Subsidy memory s = _subsidy(1);

        vm.expectEmit(address(payer));
        emit ISubsidyPayer.SubsidyPaid(1, CALLER, TO, address(token), AMOUNT);
        assertTrue(_pay(s, _sign(s)));

        assertEq(token.balanceOf(TO), AMOUNT);
        assertEq(token.balanceOf(address(payer)), BUDGET - AMOUNT);
        assertTrue(payer.isNonceUsed(1));
    }

    function testPayToRouter() public {
        Subsidy memory s = _subsidy(1);

        assertTrue(_pay(s, _sign(s), ROUTER));

        assertEq(token.balanceOf(ROUTER), AMOUNT);
    }

    function testPayTamperedField() public {
        for (uint256 field = 0; field < 7; field++) {
            Subsidy memory s = _subsidy(1);
            bytes memory sig = _sign(s);
            if (field == 0) s.boundTo = ORIGIN;
            if (field == 1) s.token = ETH_ADDRESS;
            if (field == 2) s.amount += 1;
            if (field == 3) s.minAmountIn -= 1;
            if (field == 4) s.innerSwapHash = keccak256("other swap");
            if (field == 5) s.nonce += 1;
            if (field == 6) s.deadline += 1;

            _assertSkipped(s, sig, ISubsidyPayer.SkipReason.InvalidSignature);
        }
    }

    function testPayZeroSignature() public {
        _assertSkipped(
            _subsidy(1),
            new bytes(65),
            ISubsidyPayer.SkipReason.InvalidSignature
        );
    }

    function testPayMalformedSignature() public {
        Subsidy memory s = _subsidy(1);
        bytes memory sig = _sign(s);
        assembly {
            mstore(sig, 64)
        }

        _assertSkipped(s, sig, ISubsidyPayer.SkipReason.InvalidSignature);
        _assertSkipped(s, "", ISubsidyPayer.SkipReason.InvalidSignature);
    }

    function testPayWrongSigner() public {
        (, uint256 otherPk) = makeAddrAndKey("other");
        Subsidy memory s = _subsidy(1);

        _assertSkipped(
            s, _sign(s, otherPk), ISubsidyPayer.SkipReason.InvalidSignature
        );
    }

    function testSetSigner() public {
        (address newSigner, uint256 newPk) = makeAddrAndKey("newSigner");
        Subsidy memory s = _subsidy(1);
        bytes memory oldSig = _sign(s);

        vm.expectEmit(address(payer));
        emit SubsidyPayer.SignerUpdated(SIGNER, newSigner);
        vm.prank(SIGNER_SETTER);
        payer.setSigner(newSigner);

        assertEq(payer.signer(), newSigner);
        _assertSkipped(s, oldSig, ISubsidyPayer.SkipReason.InvalidSignature);
        assertTrue(_pay(s, _sign(s, newPk)));
    }

    function testSetSignerZero() public {
        vm.prank(SIGNER_SETTER);
        vm.expectRevert(SubsidyPayer__AddressZero.selector);
        payer.setSigner(address(0));
    }

    function testPayPaused() public {
        Subsidy memory s = _subsidy(1);
        bytes memory sig = _sign(s);
        vm.prank(PAUSER_1);
        payer.pause();

        _assertSkipped(s, sig, ISubsidyPayer.SkipReason.Paused);

        vm.prank(UNPAUSER);
        payer.unpause();
        assertTrue(_pay(s, sig));
    }

    function testPausers() public {
        vm.prank(PAUSER_1);
        payer.pause();
        vm.prank(UNPAUSER);
        payer.unpause();

        vm.prank(PAUSER_2);
        payer.pause();
        assertTrue(payer.paused());
    }

    function testPauserPowers() public {
        bytes32 pauserRole = payer.PAUSER_ROLE();
        bytes32 pauserAdminRole = payer.PAUSER_ADMIN_ROLE();

        vm.startPrank(PAUSER_1);
        vm.expectRevert(_unauthorized(PAUSER_1, pauserAdminRole));
        payer.grantRole(pauserRole, CALLER);
        vm.expectRevert(_unauthorized(PAUSER_1, pauserAdminRole));
        payer.revokeRole(pauserRole, PAUSER_2);
        vm.stopPrank();

        assertTrue(payer.hasRole(pauserRole, PAUSER_2));
        assertFalse(payer.hasRole(pauserRole, CALLER));
    }

    function testPauserAdmin() public {
        bytes32 pauserRole = payer.PAUSER_ROLE();
        address newPauser = makeAddr("newPauser");

        vm.startPrank(PAUSER_ADMIN);
        payer.grantRole(pauserRole, newPauser);
        payer.revokeRole(pauserRole, PAUSER_1);
        vm.stopPrank();

        vm.prank(PAUSER_1);
        vm.expectRevert(_unauthorized(PAUSER_1, pauserRole));
        payer.pause();
        vm.prank(newPauser);
        payer.pause();
        assertTrue(payer.paused());
    }

    function testRoleSeparation() public {
        address[5] memory holders =
            [SIGNER_SETTER, BUDGET_MANAGER, PAUSER_ADMIN, UNPAUSER, PAUSER_1];
        bytes[5] memory calls = [
            abi.encodeCall(SubsidyPayer.setSigner, (CALLER)),
            abi.encodeCall(SubsidyPayer.withdraw, (address(token), CALLER, 1)),
            abi.encodeCall(SubsidyPayer.invalidateNonces, (0, 1)),
            abi.encodeCall(SubsidyPayer.unpause, ()),
            abi.encodeCall(SubsidyPayer.pause, ())
        ];
        bytes32[5] memory roles = [
            payer.SIGNER_SETTER_ROLE(),
            payer.BUDGET_ROLE(),
            payer.SIGNER_SETTER_ROLE(),
            payer.UNPAUSER_ROLE(),
            payer.PAUSER_ROLE()
        ];
        address[5] memory allowed =
            [SIGNER_SETTER, BUDGET_MANAGER, SIGNER_SETTER, UNPAUSER, PAUSER_1];

        for (uint256 c = 0; c < calls.length; c++) {
            for (uint256 h = 0; h < holders.length; h++) {
                if (holders[h] == allowed[c]) continue;
                vm.prank(holders[h]);
                (bool ok, bytes memory err) = address(payer).call(calls[c]);
                assertFalse(ok);
                assertEq(err, _unauthorized(holders[h], roles[c]));
            }
        }
    }

    function testGrantUnpauserToPauserAdmin() public {
        bytes32 unpauserRole = payer.UNPAUSER_ROLE();
        vm.prank(PAUSER_1);
        payer.pause();

        vm.prank(UNPAUSER);
        payer.grantRole(unpauserRole, PAUSER_ADMIN);
        vm.prank(PAUSER_ADMIN);
        payer.unpause();

        assertFalse(payer.paused());
    }

    function testRoleAdmins() public view {
        bytes32 defaultAdmin = payer.DEFAULT_ADMIN_ROLE();
        address[8] memory accounts = [
            address(this),
            ROUTER,
            SIGNER,
            SIGNER_SETTER,
            BUDGET_MANAGER,
            PAUSER_ADMIN,
            UNPAUSER,
            PAUSER_1
        ];
        for (uint256 i = 0; i < accounts.length; i++) {
            assertFalse(payer.hasRole(defaultAdmin, accounts[i]));
        }
        assertEq(
            payer.getRoleAdmin(payer.SIGNER_SETTER_ROLE()),
            payer.SIGNER_SETTER_ROLE()
        );
        assertEq(payer.getRoleAdmin(payer.BUDGET_ROLE()), payer.BUDGET_ROLE());
        assertEq(
            payer.getRoleAdmin(payer.UNPAUSER_ROLE()), payer.UNPAUSER_ROLE()
        );
        assertEq(
            payer.getRoleAdmin(payer.PAUSER_ADMIN_ROLE()),
            payer.PAUSER_ADMIN_ROLE()
        );
        assertEq(
            payer.getRoleAdmin(payer.PAUSER_ROLE()), payer.PAUSER_ADMIN_ROLE()
        );
    }

    function testConstructorZeroAddress() public {
        for (uint256 i = 0; i < 7; i++) {
            address router = ROUTER;
            address signer = SIGNER;
            SubsidyPayer.Roles memory roles = _roles();
            if (i == 0) router = address(0);
            if (i == 1) signer = address(0);
            if (i == 2) roles.signerSetter = address(0);
            if (i == 3) roles.budgetManager = address(0);
            if (i == 4) roles.pauserAdmin = address(0);
            if (i == 5) roles.unpauser = address(0);
            if (i == 6) roles.pausers[1] = address(0);

            vm.expectRevert(SubsidyPayer__AddressZero.selector);
            new SubsidyPayer(router, signer, roles);
        }
    }

    function testPayExpired() public {
        Subsidy memory s = _subsidy(1);
        s.deadline = block.timestamp - 1;

        _assertSkipped(s, _sign(s), ISubsidyPayer.SkipReason.Expired);
    }

    function testPayAtDeadline() public {
        Subsidy memory s = _subsidy(1);
        s.deadline = block.timestamp;

        assertTrue(_pay(s, _sign(s)));
    }

    function testPayFailingToken() public {
        ERC20Mock[2] memory tokens =
            [ERC20Mock(new FalseReturningToken()), new RevertingToken()];
        // A paid nonce in the same bitmap word must stay used when the
        // failed transfer rolls its own nonce back
        Subsidy memory paid = _subsidy(2);
        assertTrue(_pay(paid, _sign(paid)));
        for (uint256 i = 0; i < tokens.length; i++) {
            tokens[i].mint(address(payer), BUDGET);
            Subsidy memory s = _subsidy(1);
            s.token = address(tokens[i]);

            _assertSkipped(s, _sign(s), ISubsidyPayer.SkipReason.TransferFailed);
            assertEq(tokens[i].balanceOf(address(payer)), BUDGET);
            assertTrue(payer.isNonceUsed(2));
        }
    }

    function testPayUnreadableBalance() public {
        address[2] memory tokens =
            [address(new RevertingBalanceToken()), makeAddr("codeless")];
        for (uint256 i = 0; i < tokens.length; i++) {
            Subsidy memory s = _subsidy(1);
            s.token = tokens[i];

            bytes memory sig = _sign(s);

            _expectSkip(1, ISubsidyPayer.SkipReason.InsufficientBalance);
            assertFalse(_pay(s, sig));
            assertFalse(payer.isNonceUsed(1));
        }
    }

    function testPayEth() public {
        Subsidy memory s = _subsidy(1);
        s.token = ETH_ADDRESS;
        bytes memory sig = _sign(s);

        assertTrue(_pay(s, sig));

        assertEq(TO.balance, AMOUNT);
        assertEq(address(payer).balance, BUDGET - AMOUNT);
    }

    function testPayEthToRouter() public {
        Subsidy memory s = _subsidy(1);
        s.token = ETH_ADDRESS;

        assertTrue(_pay(s, _sign(s), ROUTER));

        assertEq(ROUTER.balance, AMOUNT);
    }

    function testPayEthToContract() public {
        address receiver = address(new SafeLikeReceiver());
        Subsidy memory s = _subsidy(1);
        s.token = ETH_ADDRESS;

        assertTrue(_pay(s, _sign(s), receiver));

        assertEq(receiver.balance, AMOUNT);
    }

    function testPayMarksNonceBeforeTransfer() public {
        address receiver = address(new NonceCheckingReceiver(payer, 1));
        Subsidy memory s = _subsidy(1);
        s.token = ETH_ADDRESS;
        bytes memory sig = _sign(s);

        vm.expectEmit(receiver);
        emit NonceCheckingReceiver.NonceSeen(true);
        assertTrue(_pay(s, sig, receiver));
    }

    function testPayEthInsufficientBalance() public {
        Subsidy memory s = _subsidy(1);
        s.token = ETH_ADDRESS;
        s.amount = BUDGET + 1;

        _assertSkipped(
            s, _sign(s), ISubsidyPayer.SkipReason.InsufficientBalance
        );
    }

    function testPayEthFailingReceiver() public {
        address[3] memory receivers = [
            address(new RevertingReceiver()),
            address(new GasBurningReceiver()),
            address(new HugeRevertReceiver())
        ];
        Subsidy memory s = _subsidy(1);
        s.token = ETH_ADDRESS;
        bytes memory sig = _sign(s);
        // Warm the payer's storage, so every measured call pays the same
        // storage costs
        _pay(s, sig, receivers[0]);
        uint256[3] memory gasUsed;
        for (uint256 i = 0; i < receivers.length; i++) {
            _expectSkip(1, ISubsidyPayer.SkipReason.TransferFailed);
            uint256 gasBefore = gasleft();
            assertFalse(_pay(s, sig, receivers[i]));
            gasUsed[i] = gasBefore - gasleft();

            assertEq(receivers[i].balance, 0);
            assertEq(address(payer).balance, BUDGET);
            assertFalse(payer.isNonceUsed(1));
        }
        // A receiver gets at most 22,300 gas: the 20k cap plus the EVM's 2300
        // stipend. Burning gas must cost no more than that over the plain
        // reverting receiver. A huge revert payload also costs the copy of its
        // return data, which the cap bounds to about 25k more.
        assertLt(gasUsed[1], gasUsed[0] + 22_300);
        assertLt(gasUsed[2], gasUsed[0] + 55_000);
    }

    function testReceiveAndWithdrawEth() public {
        vm.deal(CALLER, 1 ether);
        vm.prank(CALLER);
        (bool ok,) = address(payer).call{value: 1 ether}("");
        assertTrue(ok);
        assertEq(address(payer).balance, BUDGET + 1 ether);

        vm.prank(BUDGET_MANAGER);
        payer.withdraw(ETH_ADDRESS, BUDGET_MANAGER, BUDGET + 1 ether);

        assertEq(BUDGET_MANAGER.balance, BUDGET + 1 ether);
        assertEq(address(payer).balance, 0);
    }

    function testWithdrawToken() public {
        vm.expectEmit(address(payer));
        emit SubsidyPayer.Withdrawal(address(token), BUDGET, BUDGET_MANAGER);
        vm.prank(BUDGET_MANAGER);
        payer.withdraw(address(token), BUDGET_MANAGER, BUDGET);

        assertEq(token.balanceOf(BUDGET_MANAGER), BUDGET);
    }

    function testWithdrawToZero() public {
        vm.prank(BUDGET_MANAGER);
        vm.expectRevert(SubsidyPayer__AddressZero.selector);
        payer.withdraw(address(token), address(0), 1);
    }

    function testPayNotRouter() public {
        Subsidy memory s = _subsidy(1);
        bytes memory sig = _sign(s);

        vm.prank(CALLER);
        vm.expectRevert(SubsidyPayer__NotRouter.selector);
        payer.pay(s, MIN_AMOUNT_IN, CALLER, TO, sig);
    }

    function testPayBoundToOrigin() public {
        Subsidy memory s = _subsidy(1);
        s.boundTo = ORIGIN;
        bytes memory sig = _sign(s);
        address contractCaller = makeAddr("contractCaller");

        vm.prank(ROUTER, ORIGIN);
        assertTrue(payer.pay(s, MIN_AMOUNT_IN, contractCaller, TO, sig));
    }

    function testPayCallerMismatch() public {
        Subsidy memory s = _subsidy(1);
        bytes memory sig = _sign(s);
        address other = makeAddr("other");

        _expectSkip(1, ISubsidyPayer.SkipReason.CallerMismatch);
        vm.prank(ROUTER, other);
        assertFalse(payer.pay(s, MIN_AMOUNT_IN, other, TO, sig));
        assertFalse(payer.isNonceUsed(1));
        assertEq(token.balanceOf(TO), 0);

        assertTrue(_pay(s, sig));
    }

    function testPayNonceUsed() public {
        Subsidy memory s = _subsidy(1);
        bytes memory sig = _sign(s);
        assertTrue(_pay(s, sig));

        _expectSkip(1, ISubsidyPayer.SkipReason.NonceUsed);
        assertFalse(_pay(s, sig));

        assertEq(token.balanceOf(TO), AMOUNT);
    }

    function testPayBelowMinAmountIn() public {
        Subsidy memory s = _subsidy(1);
        bytes memory sig = _sign(s);

        _expectSkip(1, ISubsidyPayer.SkipReason.BelowMinAmountIn);
        vm.prank(ROUTER, ORIGIN);
        assertFalse(payer.pay(s, MIN_AMOUNT_IN - 1, CALLER, TO, sig));
        assertFalse(payer.isNonceUsed(1));
        assertEq(token.balanceOf(TO), 0);

        vm.prank(ROUTER, ORIGIN);
        assertTrue(payer.pay(s, MIN_AMOUNT_IN + 1, CALLER, TO, sig));
    }

    function testNoncesInSameWord() public {
        Subsidy memory s1 = _subsidy(256);
        Subsidy memory s2 = _subsidy(257);

        assertTrue(_pay(s1, _sign(s1)));

        assertTrue(payer.isNonceUsed(256));
        assertFalse(payer.isNonceUsed(257));
        assertFalse(payer.isNonceUsed(0));
        assertTrue(_pay(s2, _sign(s2)));
    }

    function testInvalidateNonces() public {
        Subsidy memory paid = _subsidy(256);
        bytes memory paidSig = _sign(paid);
        assertTrue(_pay(paid, paidSig));
        Subsidy memory s = _subsidy(258);
        bytes memory sig = _sign(s);

        vm.expectEmit(address(payer));
        emit SubsidyPayer.NoncesInvalidated(1, 1 << 2);
        vm.prank(SIGNER_SETTER);
        payer.invalidateNonces(1, 1 << 2);

        assertTrue(payer.isNonceUsed(258));
        assertTrue(payer.isNonceUsed(256));
        _expectSkip(258, ISubsidyPayer.SkipReason.NonceUsed);
        assertFalse(_pay(s, sig));
        _expectSkip(256, ISubsidyPayer.SkipReason.NonceUsed);
        assertFalse(_pay(paid, paidSig));
        assertEq(token.balanceOf(TO), AMOUNT);
    }

    function testPayInsufficientBalance() public {
        vm.prank(BUDGET_MANAGER);
        payer.withdraw(address(token), BUDGET_MANAGER, BUDGET - 2 * AMOUNT);
        Subsidy[3] memory subsidies = [_subsidy(1), _subsidy(2), _subsidy(3)];
        bytes[3] memory sigs =
            [_sign(subsidies[0]), _sign(subsidies[1]), _sign(subsidies[2])];

        assertTrue(_pay(subsidies[0], sigs[0]));
        assertTrue(_pay(subsidies[1], sigs[1]));
        _assertSkipped(
            subsidies[2], sigs[2], ISubsidyPayer.SkipReason.InsufficientBalance
        );

        token.mint(address(payer), AMOUNT);
        assertTrue(_pay(subsidies[2], sigs[2]));
        assertEq(token.balanceOf(TO), 3 * AMOUNT);
    }

    function testHandOverSelfAdministeredRoles() public {
        bytes32[4] memory roles = [
            payer.SIGNER_SETTER_ROLE(),
            payer.BUDGET_ROLE(),
            payer.UNPAUSER_ROLE(),
            payer.PAUSER_ADMIN_ROLE()
        ];
        address[4] memory holders =
            [SIGNER_SETTER, BUDGET_MANAGER, UNPAUSER, PAUSER_ADMIN];
        address successor = makeAddr("successor");

        for (uint256 i = 0; i < roles.length; i++) {
            vm.startPrank(holders[i]);
            payer.grantRole(roles[i], successor);
            payer.renounceRole(roles[i], holders[i]);
            vm.stopPrank();

            assertTrue(payer.hasRole(roles[i], successor));
            assertFalse(payer.hasRole(roles[i], holders[i]));
        }
    }
}
