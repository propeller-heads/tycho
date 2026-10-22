pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use store_protocol_components::store_protocol_components;

pub(crate) mod config {
    use anyhow::{anyhow, Result};

    use crate::biconomy::{
        allowance_attribute, allowance_slot, balance_attribute, balance_slot, Address, Word,
    };

    /// A token of the venue's pairs and where it keeps balances and allowances.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Token {
        pub address: Address,
        pub balance_slot: u8,
        pub allowance_slot: u8,
    }

    /// How a provider's `available(token)` is computed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ProviderKind {
        /// The provider's own token balance.
        Balance,
        /// The smaller of a vault's balance and its allowance to the provider.
        Vault(Address),
        /// No inventory cap.
        Unbounded,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Provider {
        pub address: Address,
        pub kind: ProviderKind,
        pub signer: Address,
    }

    impl Provider {
        /// The attribute value the simulation decodes: kind byte (0 balance, 1 vault,
        /// 2 unbounded), signer, then the vault (zero unless the kind is vault).
        pub fn encode(&self) -> Vec<u8> {
            let (kind, vault) = match self.kind {
                ProviderKind::Balance => (0u8, [0u8; 20]),
                ProviderKind::Vault(vault) => (1, vault),
                ProviderKind::Unbounded => (2, [0u8; 20]),
            };
            let mut out = vec![kind];
            out.extend(self.signer);
            out.extend(vault);
            out
        }
    }

    /// A starting value for a tracked balance or allowance, as of the end of `bootstrap_block`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Bootstrap {
        pub attribute: String,
        pub value: Vec<u8>,
    }

    /// Params, `&`-separated:
    /// - `venue`, `executor`: addresses.
    /// - `bootstrap_block`: the block the component is created at (the venue's deployment).
    /// - `tokens`: `address:balanceSlot:allowanceSlot`, comma-separated.
    /// - `providers`: `address:balance:signer`, `address:vault:signer:vault` or
    ///   `address:unbounded:signer`, comma-separated.
    /// - `balances`: `token:holder:value` starting values, comma-separated.
    /// - `allowances`: `token:owner:spender:value` starting values, comma-separated.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Config {
        pub venue: Address,
        pub executor: Address,
        pub bootstrap_block: u64,
        pub tokens: Vec<Token>,
        pub providers: Vec<Provider>,
        pub bootstrap: Vec<Bootstrap>,
    }

    impl Config {
        pub fn parse(params: &str) -> Result<Self> {
            let mut venue = None;
            let mut executor = None;
            let mut bootstrap_block = None;
            let mut tokens = Vec::new();
            let mut providers = Vec::new();
            let mut bootstrap = Vec::new();
            for pair in params
                .split('&')
                .filter(|part| !part.is_empty())
            {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow!("invalid param pair `{pair}`"))?;
                let entries = || {
                    value
                        .split(',')
                        .filter(|entry| !entry.is_empty())
                };
                match key {
                    "venue" => venue = Some(parse_address(value)?),
                    "executor" => executor = Some(parse_address(value)?),
                    "bootstrap_block" => bootstrap_block = Some(value.parse()?),
                    "tokens" => {
                        tokens = entries()
                            .map(parse_token)
                            .collect::<Result<_>>()?
                    }
                    "providers" => {
                        providers = entries()
                            .map(parse_provider)
                            .collect::<Result<_>>()?
                    }
                    "balances" => {
                        for entry in entries() {
                            bootstrap.push(parse_balance(entry)?);
                        }
                    }
                    "allowances" => {
                        for entry in entries() {
                            bootstrap.push(parse_allowance(entry)?);
                        }
                    }
                    _ => return Err(anyhow!("unknown Biconomy Substreams param `{key}`")),
                }
            }
            if tokens.len() < 2 {
                return Err(anyhow!("`tokens` must list at least two tokens"));
            }
            Ok(Self {
                venue: venue.ok_or_else(|| anyhow!("missing `venue` param"))?,
                executor: executor.ok_or_else(|| anyhow!("missing `executor` param"))?,
                bootstrap_block: bootstrap_block
                    .ok_or_else(|| anyhow!("missing `bootstrap_block` param"))?,
                tokens,
                providers,
                bootstrap,
            })
        }

        pub fn component_id(&self) -> String {
            crate::biconomy::hex_address(&self.venue)
        }

        /// Every token storage slot provider inventory depends on, with its attribute: each
        /// balance-kind provider's balance, and each vault's balance and allowance to its
        /// provider, for every token.
        pub fn inventory_slots(&self, token: &Token) -> Vec<(Word, String)> {
            let mut slots = Vec::new();
            for provider in &self.providers {
                match provider.kind {
                    ProviderKind::Balance => slots.push((
                        balance_slot(&provider.address, token.balance_slot),
                        balance_attribute(&token.address, &provider.address),
                    )),
                    ProviderKind::Vault(vault) => {
                        slots.push((
                            balance_slot(&vault, token.balance_slot),
                            balance_attribute(&token.address, &vault),
                        ));
                        slots.push((
                            allowance_slot(&vault, &provider.address, token.allowance_slot),
                            allowance_attribute(&token.address, &vault, &provider.address),
                        ));
                    }
                    ProviderKind::Unbounded => {}
                }
            }
            slots
        }
    }

    fn parse_address(value: &str) -> Result<Address> {
        hex::decode(value.trim_start_matches("0x"))?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("address `{value}` is not 20 bytes"))
    }

    fn parse_token(entry: &str) -> Result<Token> {
        let parts: Vec<&str> = entry.split(':').collect();
        let [address, balance_slot, allowance_slot] = parts.as_slice() else {
            return Err(anyhow!("token `{entry}` is not address:balanceSlot:allowanceSlot"));
        };
        Ok(Token {
            address: parse_address(address)?,
            balance_slot: balance_slot.parse()?,
            allowance_slot: allowance_slot.parse()?,
        })
    }

    fn parse_provider(entry: &str) -> Result<Provider> {
        let parts: Vec<&str> = entry.split(':').collect();
        let (address, kind, signer) = match parts.as_slice() {
            [address, "balance", signer] => (address, ProviderKind::Balance, signer),
            [address, "unbounded", signer] => (address, ProviderKind::Unbounded, signer),
            [address, "vault", signer, vault] => {
                (address, ProviderKind::Vault(parse_address(vault)?), signer)
            }
            _ => return Err(anyhow!("provider `{entry}` has an unknown shape")),
        };
        Ok(Provider { address: parse_address(address)?, kind, signer: parse_address(signer)? })
    }

    /// A decimal uint256 as 32 big-endian bytes (allowances are often the maximum uint256).
    fn parse_value(value: &str) -> Result<Vec<u8>> {
        if value.is_empty() ||
            !value
                .bytes()
                .all(|b| b.is_ascii_digit())
        {
            return Err(anyhow!("value `{value}` is not a decimal integer"));
        }
        let mut out = [0u8; 32];
        for digit in value
            .bytes()
            .map(|b| u16::from(b - b'0'))
        {
            let mut carry = digit;
            for byte in out.iter_mut().rev() {
                let v = u16::from(*byte) * 10 + carry;
                *byte = v as u8;
                carry = v >> 8;
            }
            if carry != 0 {
                return Err(anyhow!("value `{value}` overflows uint256"));
            }
        }
        Ok(out.to_vec())
    }

    fn parse_balance(entry: &str) -> Result<Bootstrap> {
        let parts: Vec<&str> = entry.split(':').collect();
        let [token, holder, value] = parts.as_slice() else {
            return Err(anyhow!("balance `{entry}` is not token:holder:value"));
        };
        Ok(Bootstrap {
            attribute: balance_attribute(&parse_address(token)?, &parse_address(holder)?),
            value: parse_value(value)?,
        })
    }

    fn parse_allowance(entry: &str) -> Result<Bootstrap> {
        let parts: Vec<&str> = entry.split(':').collect();
        let [token, owner, spender, value] = parts.as_slice() else {
            return Err(anyhow!("allowance `{entry}` is not token:owner:spender:value"));
        };
        Ok(Bootstrap {
            attribute: allowance_attribute(
                &parse_address(token)?,
                &parse_address(owner)?,
                &parse_address(spender)?,
            ),
            value: parse_value(value)?,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const PARAMS: &str = "venue=0x0000000000000000000000000000000000000001&\
             executor=0x0000000000000000000000000000000000000002&\
             bootstrap_block=10&\
             tokens=0x0000000000000000000000000000000000000003:3:4,0x0000000000000000000000000000000000000004:9:10&\
             providers=0x0000000000000000000000000000000000000005:balance:0x0000000000000000000000000000000000000006,\
             0x0000000000000000000000000000000000000007:vault:0x0000000000000000000000000000000000000008:0x0000000000000000000000000000000000000009&\
             balances=0x0000000000000000000000000000000000000003:0x0000000000000000000000000000000000000005:42&\
             allowances=0x0000000000000000000000000000000000000004:0x0000000000000000000000000000000000000009:0x0000000000000000000000000000000000000007:7";

        #[test]
        fn parses_params() {
            let config = Config::parse(PARAMS).unwrap();

            assert_eq!(config.venue[19], 1);
            assert_eq!(config.executor[19], 2);
            assert_eq!(config.bootstrap_block, 10);
            assert_eq!(
                config.tokens[1],
                Token { address: addr(4), balance_slot: 9, allowance_slot: 10 }
            );
            assert_eq!(config.providers[0].kind, ProviderKind::Balance);
            assert_eq!(config.providers[1].kind, ProviderKind::Vault(addr(9)));
            assert_eq!(config.bootstrap.len(), 2);
            let mut forty_two = [0u8; 32];
            forty_two[31] = 42;
            assert_eq!(config.bootstrap[0].value, forty_two.to_vec());
            assert_eq!(config.component_id(), "0x0000000000000000000000000000000000000001");
        }

        #[test]
        fn tracks_provider_balance_and_vault_balance_and_allowance() {
            let config = Config::parse(PARAMS).unwrap();
            let names: Vec<String> = config
                .inventory_slots(&config.tokens[0])
                .into_iter()
                .map(|(_, name)| name)
                .collect();

            assert_eq!(names.len(), 3);
            assert!(names[0].starts_with("balance/0x0000000000000000000000000000000000000003/0x0000000000000000000000000000000000000005"));
            assert!(names[1].ends_with("/0x0000000000000000000000000000000000000009"));
            assert!(names[2].starts_with("allowance/"));
        }

        #[test]
        fn encodes_provider_config() {
            let provider =
                Provider { address: addr(7), kind: ProviderKind::Vault(addr(9)), signer: addr(8) };
            let encoded = provider.encode();

            assert_eq!(encoded.len(), 41);
            assert_eq!(encoded[0], 1);
            assert_eq!(encoded[20], 8);
            assert_eq!(encoded[40], 9);
        }

        #[test]
        fn parses_the_maximum_uint256() {
            let max =
                "115792089237316195423570985008687907853269984665640564039457584007913129639935";
            assert_eq!(parse_value(max).unwrap(), vec![0xff; 32]);
            assert!(parse_value(&format!("{max}0")).is_err());
        }

        #[test]
        fn rejects_missing_params() {
            assert!(Config::parse("venue=0x0000000000000000000000000000000000000001").is_err());
        }

        fn addr(last: u8) -> Address {
            let mut a = [0u8; 20];
            a[19] = last;
            a
        }
    }
}

#[path = "3_map_protocol_changes.rs"]
mod map_protocol_changes;
#[path = "1_map_protocol_components.rs"]
mod map_protocol_components;
#[path = "2_store_protocol_components.rs"]
mod store_protocol_components;
