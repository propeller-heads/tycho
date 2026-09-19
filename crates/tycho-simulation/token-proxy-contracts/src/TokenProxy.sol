// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

contract TokenProxy {
    // Use a specific storage slot for the implementation address to avoid collisions
    // keccak256("PROXY_IMPLEMENTATION_SLOT")
    bytes32 private constant IMPLEMENTATION_SLOT = 0x6677c72cdeb41acaf2b17ec8a6e275c4205f27dbfe4de34ebaf2e928a7e610db;

    // Use specific storage slots for custom storage to avoid collisions with implementation
    // These slots are chosen to be far from commonly used slots and are deterministic
    // keccak256("CUSTOM_BALANCES_MAPPING")
    bytes32 private constant BALANCES_MAPPING_POSITION =
        0x474f5fd57ee674f7b6851bc6f07e751b49076dfb356356985b9daf10e9abc941;

    // keccak256("HAS_CUSTOM_BALANCE_MAPPING")
    bytes32 private constant HAS_CUSTOM_BALANCE_POSITION =
        0x7ead8ede9dbb385b0664952c7462c9938a5821e6f78e859da2e683216e99411b;

    // keccak256("CUSTOM_APPROVAL_MAPPING")
    bytes32 private constant CUSTOM_APPROVAL_MAPPING_POSITION =
        0x71a54e125991077003bef7e7ca57369c919dac6d2458895f1eab4d03960f4aeb;

    // keccak256("HAS_CUSTOM_APPROVAL_MAPPING")
    bytes32 private constant HAS_CUSTOM_APPROVAL_MAPPING_POSITION =
        0x9f0c1bc0e9c3078f9ad5fc59c8606416b3fabcbd4c8353fed22937c66c866ce3;

    // keccak256("USE_APPROVAL_SYSTEM")
    bytes32 private constant USE_APPROVAL_SYSTEM_POSITION =
        0x1c8adeb044ce7bd6c04097287b791554c6f579767c0cc52f56f479c5ae917fac;

    // Custom metadata storage slots
    //  keccak256("CUSTOM_NAME_SLOT");
    bytes32 private constant CUSTOM_NAME_POSITION = 0xcc1e513fb5bda80dc466ad9d44df38805a8dee4c82b3c6df3d9b25d3d5355d1c;
    // keccak256("CUSTOM_SYMBOL_SLOT");
    bytes32 private constant CUSTOM_SYMBOL_POSITION =
        0xdc17dd3380a9a034a702a2b2b1c6c25d39ebf0e89796e0d15e1e04d23e3bb221;
    // keccak256("CUSTOM_DECIMALS_SLOT");
    bytes32 private constant CUSTOM_DECIMALS_POSITION =
        0xadd486b234562de9ac745f036f538cda2547ef6dbb4da3fa1c017625f888a8e8;
    // keccak256("CUSTOM_TOTAL_SUPPLY_SLOT");
    bytes32 private constant CUSTOM_TOTAL_SUPPLY_POSITION =
        0x6014af1e8e9bb2844581b2fa9e5e3620181c3192eefd3258319aec23538da9f5;
    // keccak256("HAS_CUSTOM_METADATA_SLOT");
    bytes32 private constant HAS_CUSTOM_METADATA_POSITION =
        0x9f37243de61714be9cc00628d4b9bf9897ae670218af52ade6d192b4339d7616;

    // Events
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    constructor(address _original_address) {
        _setImplementation(_original_address);
    }

    // Custom metadata storage getter/setter
    function _customNameStorage() private pure returns (mapping(string => string) storage nameStorage) {
        bytes32 position = CUSTOM_NAME_POSITION;
        assembly {
            nameStorage.slot := position
        }
    }

    function _customSymbolStorage() private pure returns (mapping(string => string) storage symbolStorage) {
        bytes32 position = CUSTOM_SYMBOL_POSITION;
        assembly {
            symbolStorage.slot := position
        }
    }

    function _customDecimalsStorage() private pure returns (bytes32 position) {
        return CUSTOM_DECIMALS_POSITION;
    }

    function _customTotalSupplyStorage() private pure returns (bytes32 position) {
        return CUSTOM_TOTAL_SUPPLY_POSITION;
    }

    function _hasCustomMetadataStorage() private pure returns (mapping(string => bool) storage hasCustom) {
        bytes32 position = HAS_CUSTOM_METADATA_POSITION;
        assembly {
            hasCustom.slot := position
        }
    }

    function _hasCustomMetadata(string memory key) private view returns (bool) {
        return _hasCustomMetadataStorage()[key];
    }

    function _setHasCustomMetadata(string memory key, bool value) private {
        _hasCustomMetadataStorage()[key] = value;
    }

    // External functions to set custom metadata
    function setName(string memory newName) external {
        _customNameStorage()["name"] = newName;
        _setHasCustomMetadata("name", true);
    }

    function setSymbol(string memory newSymbol) external {
        _customSymbolStorage()["symbol"] = newSymbol;
        _setHasCustomMetadata("symbol", true);
    }

    function setDecimals(uint8 newDecimals) external {
        bytes32 position = _customDecimalsStorage();
        assembly {
            sstore(position, newDecimals)
        }
        _setHasCustomMetadata("decimals", true);
    }

    function setTotalSupply(uint256 newTotalSupply) external {
        bytes32 position = _customTotalSupplyStorage();
        assembly {
            sstore(position, newTotalSupply)
        }
        _setHasCustomMetadata("totalSupply", true);
    }

    // View functions that check for custom values first
    function name() public returns (string memory) {
        if (_hasCustomMetadata("name")) {
            return _customNameStorage()["name"];
        }

        (bool success, bytes memory data) = _implementation().delegatecall(abi.encodeWithSignature("name()"));

        if (success && data.length >= 32) {
            return abi.decode(data, (string));
        }
        return "";
    }

    function symbol() public returns (string memory) {
        if (_hasCustomMetadata("symbol")) {
            return _customSymbolStorage()["symbol"];
        }

        (bool success, bytes memory data) = _implementation().delegatecall(abi.encodeWithSignature("symbol()"));

        if (success && data.length >= 32) {
            return abi.decode(data, (string));
        }
        return "";
    }

    function decimals() public returns (uint8) {
        if (_hasCustomMetadata("decimals")) {
            bytes32 position = _customDecimalsStorage();
            uint8 value;
            assembly {
                value := sload(position)
            }
            return value;
        }

        (bool success, bytes memory data) = _implementation().delegatecall(abi.encodeWithSignature("decimals()"));

        if (success && data.length >= 32) {
            return abi.decode(data, (uint8));
        }
        return 0;
    }

    function totalSupply() public returns (uint256) {
        if (_hasCustomMetadata("totalSupply")) {
            bytes32 position = _customTotalSupplyStorage();
            uint256 value;
            assembly {
                value := sload(position)
            }
            return value;
        }

        (bool success, bytes memory data) = _implementation().delegatecall(abi.encodeWithSignature("totalSupply()"));

        if (success && data.length >= 32) {
            return abi.decode(data, (uint256));
        }
        return 0;
    }

    // Custom balance storage getter/setter using structured storage
    function _customBalanceStorage() private pure returns (mapping(address => uint256) storage balances) {
        bytes32 position = BALANCES_MAPPING_POSITION;
        assembly {
            balances.slot := position
        }
    }

    function setBalance(address account, uint256 amount) external {
        _customBalanceStorage()[account] = amount;
        _setHasCustomBalance(account, true);
    }

    function balanceOf(address account) public returns (uint256) {
        mapping(address => uint256) storage balances = _customBalanceStorage();
        if (balances[account] > 0 || _hasCustomBalance(account)) {
            return balances[account];
        }

        (bool success, bytes memory data) =
            _implementation().delegatecall(abi.encodeWithSignature("balanceOf(address)", account));

        if (success && data.length >= 32) {
            return abi.decode(data, (uint256));
        }
        return 0;
    }

    // Track which addresses have custom balances
    function _hasCustomBalanceStorage() private pure returns (mapping(address => bool) storage hasBalance) {
        bytes32 position = HAS_CUSTOM_BALANCE_POSITION;
        assembly {
            hasBalance.slot := position
        }
    }

    function _hasCustomBalance(address account) private view returns (bool) {
        return _hasCustomBalanceStorage()[account];
    }

    function _setHasCustomBalance(address account, bool value) private {
        _hasCustomBalanceStorage()[account] = value;
    }

    // Custom approval storage getter/setter using structured storage
    function _customApprovalStorage()
        private
        pure
        returns (mapping(address => mapping(address => uint256)) storage approvals)
    {
        bytes32 position = CUSTOM_APPROVAL_MAPPING_POSITION;
        assembly {
            approvals.slot := position
        }
    }

    function _hasCustomApprovalStorage() private pure returns (mapping(address => bool) storage hasApproval) {
        bytes32 position = HAS_CUSTOM_APPROVAL_MAPPING_POSITION;
        assembly {
            hasApproval.slot := position
        }
    }

    function _hasCustomApproval(address owner) private view returns (bool) {
        return _hasCustomApprovalStorage()[owner];
    }

    function _setHasCustomApproval(address owner, bool value) private {
        _hasCustomApprovalStorage()[owner] = value;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        _customApprovalStorage()[msg.sender][spender] = amount;
        _setHasCustomApproval(msg.sender, true);
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        mapping(address => uint256) storage balances = _customBalanceStorage();

        // If sender has custom balance, handle transfer locally
        if (_hasCustomBalance(msg.sender)) {
            require(balances[msg.sender] >= amount, "Insufficient balance");
            balances[msg.sender] -= amount;

            // If receiver has custom balance, update it
            if (_hasCustomBalance(to)) {
                balances[to] += amount;
            } else {
                // Get receiver's balance from implementation and set it as custom
                address impl = _implementation();
                if (impl != address(0)) {
                    (bool balanceSuccess, bytes memory balanceData) =
                        impl.delegatecall(abi.encodeWithSignature("balanceOf(address)", to));
                    require(balanceSuccess && balanceData.length >= 32, "Failed to get receiver balance");
                    uint256 receiverBalance = abi.decode(balanceData, (uint256));

                    // Set the receiver's balance as custom
                    balances[to] = receiverBalance;
                } else {
                    // If no implementation, start with 0 balance
                    balances[to] = 0;
                }

                _setHasCustomBalance(to, true);

                // Now add the transfer amount
                balances[to] += amount;
            }

            emit Transfer(msg.sender, to, amount);
            return true;
        }

        // If sender doesn't have custom balance, delegate to implementation
        (bool success,) =
            _implementation().delegatecall(abi.encodeWithSignature("transfer(address,uint256)", to, amount));

        if (success) {
            emit Transfer(msg.sender, to, amount);
            return true;
        }
        return false;
    }

    function _useApprovalSystemStorage() private pure returns (bytes32 position) {
        return USE_APPROVAL_SYSTEM_POSITION;
    }

    function _useApprovalSystem() private view returns (bool) {
        bytes32 position = _useApprovalSystemStorage();
        uint256 value;
        assembly {
            value := sload(position)
        }
        return value == 1;
    }

    function _setUseApprovalSystem(bool value) private {
        bytes32 position = _useApprovalSystemStorage();
        assembly {
            sstore(position, value)
        }
    }

    // External function to enable/disable approval system globally
    function setUseApprovalSystem(bool value) external {
        _setUseApprovalSystem(value);
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        mapping(address => uint256) storage balances = _customBalanceStorage();
        mapping(address => mapping(address => uint256)) storage approvals = _customApprovalStorage();

        // Check if we have custom balances or approvals
        bool hasCustomFromBalance = _hasCustomBalance(from);
        bool hasCustomToBalance = _hasCustomBalance(to);
        bool hasCustomApproval = _hasCustomApproval(from);

        // If we have custom balances and approvals, handle locally
        if (hasCustomFromBalance && hasCustomApproval) {
            require(balances[from] >= amount, "Insufficient balance");

            // Only check allowance if the approval system is enabled globally
            if (_useApprovalSystem()) {
                require(approvals[from][msg.sender] >= amount, "Insufficient allowance");
                approvals[from][msg.sender] -= amount;
            }

            balances[from] -= amount;

            // Handle receiver's balance
            if (hasCustomToBalance) {
                balances[to] += amount;
            } else {
                address impl = _implementation();
                if (impl != address(0)) {
                    // Get receiver's balance from implementation and set it as custom
                    (bool balanceSuccess, bytes memory balanceData) =
                        impl.delegatecall(abi.encodeWithSignature("balanceOf(address)", to));
                    require(balanceSuccess && balanceData.length >= 32, "Failed to get receiver balance");
                    uint256 receiverBalance = abi.decode(balanceData, (uint256));

                    // Set the receiver's balance as custom
                    balances[to] = receiverBalance;
                } else {
                    // If no implementation, start with 0 balance
                    balances[to] = 0;
                }

                _setHasCustomBalance(to, true);

                // Now add the transfer amount
                balances[to] += amount;
            }

            emit Transfer(from, to, amount);
            return true;
        }

        // If we don't have custom balances/approvals, delegate to implementation
        (bool success,) = _implementation()
            .delegatecall(abi.encodeWithSignature("transferFrom(address,address,uint256)", from, to, amount));

        if (success) {
            emit Transfer(from, to, amount);
            return true;
        }
        return false;
    }

    function allowance(address owner, address spender) public returns (uint256) {
        if (_hasCustomApproval(owner)) {
            uint256 approved = _customApprovalStorage()[owner][spender];

            // Caveat this trades off: with the approval system disabled, simulation now *assumes*
            // spendability rather than checking it. A protocol that gates quoting on its own
            // standing approval will be quoted even if that approval has been revoked on chain.
            // That matches how `transferFrom` already behaves here, and the alternative -- report
            // zero and refuse to quote anything -- is strictly worse, but it means an allowance
            // read is not a safety check under simulation.
            //
            // An explicit in-simulation `approve` wins. Otherwise mirror `transferFrom`: with the
            // approval system globally disabled, a holder flagged for custom approval can be spent
            // from without any allowance at all, so returning the unpopulated mapping's zero
            // contradicts what a transfer would actually do. Report the unlimited allowance
            // `transferFrom` would honour instead — protocols that gate quoting on an `allowance`
            // read (e.g. a pAMM checking its own vault's standing approval before pricing) would
            // otherwise refuse to quote against state that can in fact be spent.
            if (approved == 0 && !_useApprovalSystem()) {
                return type(uint256).max;
            }
            return approved;
        }

        (bool success, bytes memory data) =
            _implementation().delegatecall(abi.encodeWithSignature("allowance(address,address)", owner, spender));

        if (success && data.length >= 32) {
            return abi.decode(data, (uint256));
        }
        return 0;
    }

    // Implementation getter/setter
    function _implementation() private view returns (address implementation) {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly {
            implementation := sload(slot)
        }
    }

    function _setImplementation(address _impl) private {
        bytes32 slot = IMPLEMENTATION_SLOT;
        assembly {
            sstore(slot, _impl)
        }
    }

    // Fallback to forward all other calls
    fallback() external payable {
        address _impl = _implementation();
        (bool success, bytes memory data) = _impl.delegatecall(msg.data);

        if (!success) {
            assembly {
                revert(add(data, 32), mload(data))
            }
        }

        assembly {
            return(add(data, 32), mload(data))
        }
    }

    receive() external payable {}
}
