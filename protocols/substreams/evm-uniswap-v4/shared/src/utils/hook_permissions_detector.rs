use ethabi::ethereum_types::Address;

/// Detector for Uniswap V4 hook permissions encoded in hook contract addresses.
///
/// In Uniswap V4, hook permissions are encoded in the least significant bits of the hook contract
/// address. Each bit represents a specific hook permission that determines which functions the hook
/// can implement. This struct provides utilities to decode and check these permissions from hook
/// addresses.
///
/// # Hook Permission Flags
///
/// The following flags are defined for swap-related hooks:
/// - `BEFORE_SWAP_FLAG` (bit 7): Permission to implement the `beforeSwap` hook
/// - `AFTER_SWAP_FLAG` (bit 6): Permission to implement the `afterSwap` hook
pub struct HookPermissionsDetector {}

impl HookPermissionsDetector {
    /// Flag for the beforeSwap hook permission (bit 7)
    const BEFORE_SWAP_FLAG: u32 = 1 << 7;

    /// Flag for the afterSwap hook permission (bit 6)
    const AFTER_SWAP_FLAG: u32 = 1 << 6;

    /// Extracts the least significant 32 bits from an Ethereum address for hook flag checking.
    ///
    /// # Arguments
    ///
    /// * `address` - The Ethereum address to extract flags from
    ///
    /// # Returns
    ///
    /// A u32 containing the last 4 bytes of the address
    fn get_address_flags(address: &Address) -> u32 {
        let bytes = address.as_bytes();
        // Take the last 4 bytes (32 bits) of the address and convert to u32
        // Ethereum addresses are 20 bytes, so we take bytes 16-19 (0-indexed)
        let flag_bytes = &bytes[16..20];
        u32::from_be_bytes(
            flag_bytes
                .try_into()
                .expect("slice with incorrect length"),
        )
    }

    /// Checks if a specific hook permission flag is set in the address.
    ///
    /// # Arguments
    ///
    /// * `address` - The hook contract address to check
    /// * `flag` - The permission flag to check for
    ///
    /// # Returns
    ///
    /// `true` if the flag is set, `false` otherwise
    fn has_permission(address: &Address, flag: u32) -> bool {
        let flags = Self::get_address_flags(address);
        (flags & flag) != 0
    }

    /// Checks if the hook address has the `beforeSwap` hook permission enabled.
    ///
    /// # Arguments
    ///
    /// * `address` - The hook contract address to check
    ///
    /// # Returns
    ///
    /// `true` if the beforeSwap permission (bit 7) is set
    pub fn has_before_swap_hook(address: &Address) -> bool {
        Self::has_permission(address, Self::BEFORE_SWAP_FLAG)
    }

    /// Checks if the hook address has the `afterSwap` hook permission enabled.
    ///
    /// # Arguments
    ///
    /// * `address` - The hook contract address to check
    ///
    /// # Returns
    ///
    /// `true` if the afterSwap permission (bit 6) is set
    pub fn has_after_swap_hook(address: &Address) -> bool {
        Self::has_permission(address, Self::AFTER_SWAP_FLAG)
    }

    /// Checks if the hook address has either `beforeSwap` or `afterSwap` hook permissions.
    ///
    /// This is a convenience method that returns true if either swap hook is enabled.
    ///
    /// # Arguments
    ///
    /// * `address` - The hook contract address to check
    ///
    /// # Returns
    ///
    /// `true` if either beforeSwap or afterSwap permission is set
    pub fn has_swap_hooks(address: &Address) -> bool {
        let has_before_swap = Self::has_before_swap_hook(address);
        let has_after_swap = Self::has_after_swap_hook(address);
        has_before_swap || has_after_swap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_hook_with_swap_permissions() {
        // Test with 0x8dAaE9702d913633E639CAB6B494A3779F89E8A8 - known hook with swap hooks
        let hook_address = Address::from_str("0x8dAaE9702d913633E639CAB6B494A3779F89E8A8").unwrap();

        // This address ends with 0xE8A8, which in binary is:
        // 1110 1000 1010 1000
        // Bit 7 (beforeSwap) = 1, Bit 6 (afterSwap) = 0
        assert!(HookPermissionsDetector::has_before_swap_hook(&hook_address));
        assert!(!HookPermissionsDetector::has_after_swap_hook(&hook_address));
        assert!(HookPermissionsDetector::has_swap_hooks(&hook_address));
    }

    #[test]
    fn test_zero_address_no_permissions() {
        // Test with 0x0000000000000000000000000000000000000000 - should have no swap hooks
        let zero_address = Address::from_str("0x0000000000000000000000000000000000000000").unwrap();

        assert!(!HookPermissionsDetector::has_before_swap_hook(&zero_address));
        assert!(!HookPermissionsDetector::has_after_swap_hook(&zero_address));
        assert!(!HookPermissionsDetector::has_swap_hooks(&zero_address));
    }

    #[test]
    fn test_hook_without_swap_permissions() {
        // Test with address that has beforeInitialize flag but no swap hooks
        // 0x0000000000000000000000000000000000002000 has bit 13 set (beforeInitialize)
        let hook_address = Address::from_str("0x0000000000000000000000000000000000002000").unwrap();

        assert!(!HookPermissionsDetector::has_before_swap_hook(&hook_address));
        assert!(!HookPermissionsDetector::has_after_swap_hook(&hook_address));
        assert!(!HookPermissionsDetector::has_swap_hooks(&hook_address));
    }

    #[test]
    fn test_both_swap_hooks_enabled() {
        // Test address with both beforeSwap (bit 7) and afterSwap (bit 6) set
        // 0x00000000000000000000000000000000000000C0 = 0b11000000
        let hook_address = Address::from_str("0x00000000000000000000000000000000000000C0").unwrap();

        assert!(HookPermissionsDetector::has_before_swap_hook(&hook_address));
        assert!(HookPermissionsDetector::has_after_swap_hook(&hook_address));
        assert!(HookPermissionsDetector::has_swap_hooks(&hook_address));
    }

    #[test]
    fn test_only_after_swap_hook() {
        // Test address with only afterSwap (bit 6) set
        // 0x0000000000000000000000000000000000000040 = 0b01000000
        let hook_address = Address::from_str("0x0000000000000000000000000000000000000040").unwrap();

        assert!(!HookPermissionsDetector::has_before_swap_hook(&hook_address));
        assert!(HookPermissionsDetector::has_after_swap_hook(&hook_address));
        assert!(HookPermissionsDetector::has_swap_hooks(&hook_address));
    }
}
