use std::{collections::HashMap, str::FromStr, sync::LazyLock};

use alloy::primitives::{Address, U256};

use super::utils::get_storage_slot_index_at_key;
use crate::evm::{ContractCompiler, SlotId};

pub(crate) type Overwrites = HashMap<SlotId, U256>;

pub static IMPLEMENTATION_SLOT: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x6677C72CDEB41ACAF2B17EC8A6E275C4205F27DBFE4DE34EBAF2E928A7E610DB").unwrap()
});
static BALANCES_MAPPING_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x474F5FD57EE674F7B6851BC6F07E751B49076DFB356356985B9DAF10E9ABC941").unwrap()
});
static HAS_CUSTOM_BALANCE_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x7EAD8EDE9DBB385B0664952C7462C9938A5821E6F78E859DA2E683216E99411B").unwrap()
});
static CUSTOM_APPROVAL_MAPPING_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x71A54E125991077003BEF7E7CA57369C919DAC6D2458895F1EAB4D03960F4AEB").unwrap()
});
static HAS_CUSTOM_APPROVAL_MAPPING_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x9F0C1BC0E9C3078F9AD5FC59C8606416B3FABCBD4C8353FED22937C66C866CE3").unwrap()
});
static CUSTOM_NAME_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0xCC1E513FB5BDA80DC466AD9D44DF38805A8DEE4C82B3C6DF3D9B25D3D5355D1C").unwrap()
});
static CUSTOM_SYMBOL_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0xDC17DD3380A9A034A702A2B2B1C6C25D39EBF0E89796E0D15E1E04D23E3BB221").unwrap()
});
static CUSTOM_DECIMALS_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0xADD486B234562DE9AC745F036F538CDA2547EF6DBB4DA3FA1C017625F888A8E8").unwrap()
});
static CUSTOM_TOTAL_SUPPLY_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x6014AF1E8E9BB2844581B2FA9E5E3620181C3192EEFD3258319AEC23538DA9F5").unwrap()
});
static HAS_CUSTOM_METADATA_POSITION: LazyLock<SlotId> = LazyLock::new(|| {
    U256::from_str("0x9F37243DE61714BE9CC00628D4B9BF9897AE670218AF52ADE6D192B4339D7616").unwrap()
});

pub(crate) struct TokenProxyOverwriteFactory {
    token_address: Address,
    overwrites: Overwrites,
    compiler: ContractCompiler,
}

impl TokenProxyOverwriteFactory {
    pub(crate) fn new(token_address: Address, proxy_address: Option<Address>) -> Self {
        let mut instance = Self {
            token_address,
            overwrites: HashMap::new(),
            compiler: ContractCompiler::Solidity,
        };

        if let Some(proxy_addr) = proxy_address {
            instance.set_original_address(proxy_addr);
        }

        instance
    }

    /// Sets the original address of the token contract in the implementation slot.
    pub(crate) fn set_original_address(&mut self, implementation: Address) {
        self.overwrites
            .insert(*IMPLEMENTATION_SLOT, U256::from_be_slice(implementation.as_slice()));
    }

    pub(crate) fn set_balance(&mut self, balance: U256, owner: Address) {
        // Set the balance in the custom storage slot
        let storage_index =
            get_storage_slot_index_at_key(owner, *BALANCES_MAPPING_POSITION, self.compiler);
        self.overwrites
            .insert(storage_index, balance);

        // Set the has_custom_balance flag to true
        let has_balance_index =
            get_storage_slot_index_at_key(owner, *HAS_CUSTOM_BALANCE_POSITION, self.compiler);
        self.overwrites
            .insert(has_balance_index, U256::from(1)); // true in Solidity
    }

    pub(crate) fn set_allowance(&mut self, allowance: U256, spender: Address, owner: Address) {
        // Set the allowance in the custom storage slot
        let owner_slot =
            get_storage_slot_index_at_key(owner, *CUSTOM_APPROVAL_MAPPING_POSITION, self.compiler);
        let storage_index = get_storage_slot_index_at_key(spender, owner_slot, self.compiler);
        self.overwrites
            .insert(storage_index, allowance);

        // Set the has_custom_approval flag to true
        let has_approval_index = get_storage_slot_index_at_key(
            owner,
            *HAS_CUSTOM_APPROVAL_MAPPING_POSITION,
            self.compiler,
        );
        self.overwrites
            .insert(has_approval_index, U256::from(1)); // true in Solidity
    }

    /// Marks `owner` as having custom approvals, without granting a specific allowance.
    ///
    /// `transferFrom` from this owner is then handled by the proxy's local balance bookkeeping
    /// instead of delegating to the implementation. The proxy's approval system is disabled by
    /// default, so the local branch does not check a per-spender amount — only this flag is needed.
    pub(crate) fn set_has_custom_approval(&mut self, owner: Address) {
        let has_approval_index = get_storage_slot_index_at_key(
            owner,
            *HAS_CUSTOM_APPROVAL_MAPPING_POSITION,
            self.compiler,
        );
        self.overwrites
            .insert(has_approval_index, U256::from(1)); // true in Solidity
    }

    #[allow(dead_code)]
    pub(crate) fn set_total_supply(&mut self, supply: U256) {
        self.overwrites
            .insert(*CUSTOM_TOTAL_SUPPLY_POSITION, supply);
    }

    /// Sets the has_custom_metadata flag for a given key
    #[allow(dead_code)]
    fn set_metadata_flag(&mut self, key: &str) {
        let key_bytes = string_to_storage_bytes(key);
        let mapping_slot_bytes: [u8; 32] = HAS_CUSTOM_METADATA_POSITION.to_be_bytes();
        let has_metadata_index = self
            .compiler
            .compute_map_slot(&key_bytes, &mapping_slot_bytes);
        self.overwrites
            .insert(has_metadata_index, U256::from(1)); // true in Solidity
    }

    #[allow(dead_code)]
    pub(crate) fn set_name(&mut self, name: &str) {
        // Store the name value
        let name_value = U256::from_be_bytes(string_to_storage_bytes(name));
        self.overwrites
            .insert(*CUSTOM_NAME_POSITION, name_value);

        // Set the has_custom_metadata flag for name to true
        self.set_metadata_flag("name");
    }

    #[allow(dead_code)]
    pub(crate) fn set_symbol(&mut self, symbol: &str) {
        // Store the symbol value
        let symbol_value = U256::from_be_bytes(string_to_storage_bytes(symbol));
        self.overwrites
            .insert(*CUSTOM_SYMBOL_POSITION, symbol_value);

        // Set the has_custom_metadata flag for symbol to true
        self.set_metadata_flag("symbol");
    }

    #[allow(dead_code)]
    pub(crate) fn set_decimals(&mut self, decimals: u8) {
        self.overwrites
            .insert(*CUSTOM_DECIMALS_POSITION, U256::from(decimals));

        // Set the has_custom_metadata flag for decimals to true
        self.set_metadata_flag("decimals");
    }

    pub(crate) fn get_overwrites(&self) -> HashMap<Address, Overwrites> {
        let mut result = HashMap::new();
        result.insert(self.token_address, self.overwrites.clone());
        result
    }
}

/// Converts a string to a 32-byte array for storage, truncating if necessary
pub fn string_to_storage_bytes(s: &str) -> [u8; 32] {
    let mut padded = [0u8; 32];
    let len = s.len().min(31);
    padded[..len].copy_from_slice(&s.as_bytes()[..len]);
    padded[31] = (len * 2) as u8; // Length * 2 for short strings
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_metadata_slot(key: &str) -> SlotId {
        let key_bytes = string_to_storage_bytes(key);
        let mapping_slot_bytes: [u8; 32] = HAS_CUSTOM_METADATA_POSITION.to_be_bytes();
        ContractCompiler::Solidity.compute_map_slot(&key_bytes, &mapping_slot_bytes)
    }

    #[test]
    fn test_token_proxy_factory_new() {
        let token_address = Address::random();
        let factory = TokenProxyOverwriteFactory::new(token_address, None);
        assert_eq!(factory.token_address, token_address);
        assert!(factory.overwrites.is_empty());
    }

    #[test]
    fn test_token_proxy_factory_with_implementation() {
        let token_address = Address::random();
        let implementation = Address::random();
        let factory = TokenProxyOverwriteFactory::new(token_address, Some(implementation));

        // Check if implementation was set correctly
        let mut expected_bytes = [0u8; 32];
        expected_bytes[12..].copy_from_slice(implementation.as_slice());
        let expected_value = U256::from_be_bytes(expected_bytes);

        assert_eq!(factory.overwrites[&*IMPLEMENTATION_SLOT], expected_value);
    }

    #[test]
    fn test_token_proxy_set_balance() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let owner = Address::random();
        let balance = U256::from(1000);

        factory.set_balance(balance, owner);

        // Check balance storage
        let storage_index =
            get_storage_slot_index_at_key(owner, *BALANCES_MAPPING_POSITION, factory.compiler);
        assert_eq!(factory.overwrites[&storage_index], balance);

        // Check has_custom_balance flag
        let has_balance_index =
            get_storage_slot_index_at_key(owner, *HAS_CUSTOM_BALANCE_POSITION, factory.compiler);
        assert_eq!(factory.overwrites[&has_balance_index], U256::from(1));
    }

    #[test]
    fn test_token_proxy_set_allowance() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let owner = Address::random();
        let spender = Address::random();
        let allowance = U256::from(500);

        factory.set_allowance(allowance, spender, owner);

        // Check allowance storage
        let owner_slot = get_storage_slot_index_at_key(
            owner,
            *CUSTOM_APPROVAL_MAPPING_POSITION,
            factory.compiler,
        );
        let storage_index = get_storage_slot_index_at_key(spender, owner_slot, factory.compiler);
        assert_eq!(factory.overwrites[&storage_index], allowance);

        // Check has_custom_approval flag
        let has_approval_index = get_storage_slot_index_at_key(
            owner,
            *HAS_CUSTOM_APPROVAL_MAPPING_POSITION,
            factory.compiler,
        );
        assert_eq!(factory.overwrites[&has_approval_index], U256::from(1));
    }

    #[test]
    fn test_token_proxy_set_total_supply() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let supply = U256::from(1_000_000);

        factory.set_total_supply(supply);

        assert_eq!(factory.overwrites[&*CUSTOM_TOTAL_SUPPLY_POSITION], supply);
    }

    #[test]
    fn test_token_proxy_set_name() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let name = "Test Token";

        factory.set_name(name);

        // Check name storage
        let mut expected_bytes = [0u8; 32];
        let name_bytes = name.as_bytes();
        expected_bytes[..name_bytes.len()].copy_from_slice(name_bytes);
        expected_bytes[31] = (name_bytes.len() * 2) as u8; // Length * 2 for short strings
        let expected_value = U256::from_be_bytes(expected_bytes);
        assert_eq!(factory.overwrites[&*CUSTOM_NAME_POSITION], expected_value);

        // Check has_custom_metadata flag
        let has_metadata_index = get_metadata_slot("name");
        assert_eq!(factory.overwrites[&has_metadata_index], U256::from(1));
    }

    #[test]
    fn test_token_proxy_set_symbol() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let symbol = "TEST";

        factory.set_symbol(symbol);

        // Check symbol storage
        let mut expected_bytes = [0u8; 32];
        let symbol_bytes = symbol.as_bytes();
        expected_bytes[..symbol_bytes.len()].copy_from_slice(symbol_bytes);
        expected_bytes[31] = (symbol_bytes.len() * 2) as u8; // Length * 2 for short strings
        let expected_value = U256::from_be_bytes(expected_bytes);
        assert_eq!(factory.overwrites[&*CUSTOM_SYMBOL_POSITION], expected_value);

        // Check has_custom_metadata flag
        let has_metadata_index = get_metadata_slot("symbol");
        assert_eq!(factory.overwrites[&has_metadata_index], U256::from(1));
    }

    #[test]
    fn test_token_proxy_set_decimals() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let decimals = 18u8;

        factory.set_decimals(decimals);

        assert_eq!(factory.overwrites[&*CUSTOM_DECIMALS_POSITION], U256::from(decimals));

        // Check has_custom_metadata flag
        let has_metadata_index = get_metadata_slot("decimals");
        assert_eq!(factory.overwrites[&has_metadata_index], U256::from(1));
    }

    #[test]
    fn test_token_proxy_get_overwrites() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let supply = U256::from(1_000_000);
        factory.set_total_supply(supply);

        let overwrites = factory.get_overwrites();

        assert_eq!(overwrites.len(), 1);
        assert!(overwrites.contains_key(&factory.token_address));
        assert_eq!(overwrites[&factory.token_address][&*CUSTOM_TOTAL_SUPPLY_POSITION], supply);
    }

    #[test]
    fn test_token_proxy_set_long_name_truncated() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let name = "This is a very long token name that exceeds 31 bytes";

        factory.set_name(name);

        // Check name storage for truncated string
        let mut expected_bytes = [0u8; 32];
        expected_bytes[..31].copy_from_slice(&name.as_bytes()[..31]);
        expected_bytes[31] = 62; // 31 * 2 for short strings
        let expected_value = U256::from_be_bytes(expected_bytes);
        assert_eq!(factory.overwrites[&*CUSTOM_NAME_POSITION], expected_value);

        // Check has_custom_metadata flag
        let has_metadata_index = get_metadata_slot("name");
        assert_eq!(factory.overwrites[&has_metadata_index], U256::from(1));
    }

    #[test]
    fn test_token_proxy_set_long_symbol_truncated() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);
        let symbol = "This is a very long token symbol that exceeds 31 bytes";

        factory.set_symbol(symbol);

        // Check symbol storage for truncated string
        let mut expected_bytes = [0u8; 32];
        expected_bytes[..31].copy_from_slice(&symbol.as_bytes()[..31]);
        expected_bytes[31] = 62; // 31 * 2 for short strings
        let expected_value = U256::from_be_bytes(expected_bytes);
        assert_eq!(factory.overwrites[&*CUSTOM_SYMBOL_POSITION], expected_value);

        // Check has_custom_metadata flag
        let has_metadata_index = get_metadata_slot("symbol");
        assert_eq!(factory.overwrites[&has_metadata_index], U256::from(1));
    }

    #[test]
    fn test_string_to_storage_bytes() {
        // Test short string
        let short = "Test";
        let bytes = string_to_storage_bytes(short);
        assert_eq!(bytes[..4], short.as_bytes()[..4]);
        assert_eq!(bytes[31], 8); // 4 * 2 for length

        // Test long string (should be truncated)
        let long = "This is a very long string that exceeds 31 bytes";
        let bytes = string_to_storage_bytes(long);
        assert_eq!(bytes[..31], long.as_bytes()[..31]);
        assert_eq!(bytes[31], 62); // 31 * 2 for length
    }

    #[test]
    fn test_set_metadata_flag() {
        let mut factory = TokenProxyOverwriteFactory::new(Address::random(), None);

        // Test setting metadata flag for a key
        factory.set_metadata_flag("test_key");

        // Verify the flag was set correctly
        let key_bytes = string_to_storage_bytes("test_key");
        let mapping_slot_bytes: [u8; 32] = HAS_CUSTOM_METADATA_POSITION.to_be_bytes();
        let has_metadata_index =
            ContractCompiler::Solidity.compute_map_slot(&key_bytes, &mapping_slot_bytes);
        assert_eq!(factory.overwrites[&has_metadata_index], U256::from(1));
    }
}
