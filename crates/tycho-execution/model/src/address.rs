use serde::Serialize;

/// Models relevant addresses like `msg.sender` or `WETH`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
pub enum Address {
    /// `address(0)`
    #[serde(rename = "address(0)")]
    Zero,
    #[serde(rename = "TychoRouter")]
    /// The router's address. `address(this)` inside the router.
    Router,
    #[serde(rename = "msg.sender")]
    /// `msg.sender`
    Sender,
    /// Some other address `msg.sender` controls
    #[serde(rename = "sender-controlled")]
    SenderControlled,
    /// We choose WETH as the only value holding ERC20
    /// as its value directly matches the value of [Address::NativeETH].
    /// this makes it much easier to determine whether a user
    /// managed to steal value in scenarios that involve both
    /// ERC20 and the native token
    WETH,
    /// The canonical ETH marker address used by the TychoRouter.
    /// <https://etherscan.io/address/0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE>
    NativeETH,
    /// Address chosen by sender to receive client fee.
    ClientFeeReceiver,
    /// Address chosen by tycho to receive router fee.
    RouterFeeReceiver,
    /// Sometimes other addresses are needed. they can be represented using this variant
    #[serde(untagged)]
    Named(&'static str),
}

impl Address {
    pub const VARIANTS: [Address; 6] = [
        Address::Zero, /* No longer represents NativeETH, but we should still ensure this
                        * doesn't cause issues */
        Address::Router,
        Address::Sender,
        Address::SenderControlled,
        Address::WETH,
        Address::NativeETH,
    ];

    pub const VARIANTS_EXCEPT_ZERO: [Address; 5] = [
        Address::Router,
        Address::Sender,
        Address::SenderControlled,
        Address::WETH,
        Address::NativeETH,
    ];

    /// Addresses which do or could implement ERC20,
    /// plus [Address::NativeETH] for the native currency.
    /// Using this instead of [Address::VARIANTS]
    /// might miss edge case vulnerabilities
    /// but will reduce the parameter space,
    /// reduce the potential number of reverts due to the address not implementing ERC20,
    /// and speed up the simulation.
    pub const POSSIBLY_ERC20_AND_NATIVE: [Address; 4] =
        [Address::Sender, Address::SenderControlled, Address::WETH, Address::NativeETH];

    pub const SENDER_CONTROLLED: [Address; 2] = [Address::Sender, Address::SenderControlled];

    /// Whether the sender owns the [Address]'s private key
    /// and can make the [Address] do anything.
    /// This includes the [Address] being a contract that implements arbitrary code.
    pub fn is_sender_controlled(&self) -> bool {
        // the sender can freely choose ClientFeeReceiver
        self == &Address::Sender ||
            self == &Address::SenderControlled ||
            self == &Address::ClientFeeReceiver
    }

    /// ERC20 operations should revert for addresses that never implement ERC20
    pub fn is_never_erc20(&self) -> bool {
        self == &Address::Zero || self == &Address::Router || self == &Address::NativeETH
    }
}
