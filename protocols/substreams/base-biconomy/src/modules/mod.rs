pub use map_protocol_changes::map_protocol_changes;
pub use map_protocol_components::map_protocol_components;
pub use map_state_writes::map_state_writes;
pub use store_protocol_components::store_protocol_components;
pub use store_state::store_state;

pub(crate) mod config {
    use anyhow::{anyhow, Result};

    use crate::biconomy::{
        address_word, allowance_attribute, allowance_slot, approved_executor_attribute,
        array_slots, b20_allowance_slot, b20_balance_slot, balance_attribute, balance_slot, root,
        router_attribute, router_count_attribute, signer_attribute, solady_allowance_slot,
        solady_balance_slot, Address, Word,
    };

    /// A tracked token and where it keeps balances and allowances.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Token {
        pub address: Address,
        pub layout: TokenLayout,
    }

    /// How a token lays out its balances and allowances in storage.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TokenLayout {
        /// Solidity mappings: `balanceOf` at `balance_slot`, `allowance` at `allowance_slot`.
        Mapping { balance_slot: u8, allowance_slot: u8 },
        /// Solady's `ERC20`, which derives both from fixed seeds.
        Solady,
        /// A B20 token (Base's node-native standard): mappings in the `base.b20` namespace.
        B20,
    }

    impl Token {
        pub fn balance_slot(&self, holder: &Address) -> Word {
            match self.layout {
                TokenLayout::Mapping { balance_slot: base, .. } => balance_slot(holder, base),
                TokenLayout::Solady => solady_balance_slot(holder),
                TokenLayout::B20 => b20_balance_slot(holder),
            }
        }

        pub fn allowance_slot(&self, owner: &Address, spender: &Address) -> Word {
            match self.layout {
                TokenLayout::Mapping { allowance_slot: base, .. } => {
                    allowance_slot(owner, spender, base)
                }
                TokenLayout::Solady => solady_allowance_slot(owner, spender),
                TokenLayout::B20 => b20_allowance_slot(owner, spender),
            }
        }
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
        /// `signer()` at `bootstrap_block`, or for good when it is immutable.
        pub signer: Address,
        /// Storage slots of `signer` and `approvedExecutor`, when the provider can change them.
        pub slots: Option<ProviderSlots>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ProviderSlots {
        pub signer: u8,
        pub approved_executor: u8,
    }

    impl Provider {
        /// The attribute value the simulation decodes: kind byte (0 balance, 1 vault,
        /// 2 unbounded), then the vault (zero unless the kind is vault).
        pub fn encode(&self) -> Vec<u8> {
            let (kind, vault) = match self.kind {
                ProviderKind::Balance => (0u8, [0u8; 20]),
                ProviderKind::Vault(vault) => (1, vault),
                ProviderKind::Unbounded => (2, [0u8; 20]),
            };
            let mut out = vec![kind];
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
    /// - `tokens`: `address:balanceSlot:allowanceSlot`, `address:solady` for a token on Solady's
    ///   `ERC20`, or `address:b20` for a B20 token, comma-separated. Tokens can be listed ahead of
    ///   their pairs.
    /// - `providers`: `address:balance:signer`, `address:vault:signer:vault` or
    ///   `address:unbounded:signer`, comma-separated. Every listed provider starts out naming
    ///   `signer` and approving `executor`. Providers in `provider_slots` are then followed from
    ///   their storage writes, including a deployment after `bootstrap_block`; any other provider
    ///   must keep both for good.
    /// - `provider_slots`: `provider:signerSlot:approvedExecutorSlot` for providers that can change
    ///   their signer or executor, comma-separated. Others are taken as immutable.
    /// - `vault_routers`: `vault:slot` where a vault keeps `router()` as the first element of a
    ///   dynamic array, comma-separated. A vault without an entry keeps its provider as router.
    /// - `balances`: `token:holder:value` starting values, comma-separated.
    /// - `allowances`: `token:owner:spender:value` starting values, comma-separated.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Config {
        pub venue: Address,
        pub executor: Address,
        pub bootstrap_block: u64,
        pub tokens: Vec<Token>,
        pub providers: Vec<Provider>,
        pub vault_routers: Vec<(Address, u8)>,
        pub bootstrap: Vec<Bootstrap>,
    }

    impl Config {
        pub fn parse(params: &str) -> Result<Self> {
            let mut venue = None;
            let mut executor = None;
            let mut bootstrap_block = None;
            let mut tokens = Vec::new();
            let mut providers: Vec<Provider> = Vec::new();
            let mut provider_slots = Vec::new();
            let mut vault_routers = Vec::new();
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
                    "provider_slots" => {
                        provider_slots = entries()
                            .map(parse_provider_slots)
                            .collect::<Result<_>>()?
                    }
                    "vault_routers" => {
                        vault_routers = entries()
                            .map(parse_vault_router)
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
            for (address, slots) in provider_slots {
                providers
                    .iter_mut()
                    .find(|provider| provider.address == address)
                    .ok_or_else(|| anyhow!("`provider_slots` names an unlisted provider"))?
                    .slots = Some(slots);
            }
            Ok(Self {
                venue: venue.ok_or_else(|| anyhow!("missing `venue` param"))?,
                executor: executor.ok_or_else(|| anyhow!("missing `executor` param"))?,
                bootstrap_block: bootstrap_block
                    .ok_or_else(|| anyhow!("missing `bootstrap_block` param"))?,
                tokens,
                providers,
                vault_routers,
                bootstrap,
            })
        }

        pub fn tracks_token(&self, token: &Address) -> bool {
            self.tokens
                .iter()
                .any(|t| t.address == *token)
        }

        /// Every provider and vault storage slot that decides whether a provider serves the
        /// venue, by contract, with its attribute.
        pub fn provider_state_slots(&self) -> Vec<(Address, Vec<(Word, String)>)> {
            let mut out = Vec::new();
            for provider in &self.providers {
                if let Some(slots) = provider.slots {
                    out.push((
                        provider.address,
                        vec![
                            (root(slots.signer), signer_attribute(&provider.address)),
                            (
                                root(slots.approved_executor),
                                approved_executor_attribute(&provider.address),
                            ),
                        ],
                    ));
                }
            }
            for (vault, slot) in &self.vault_routers {
                let (length, first) = array_slots(*slot);
                out.push((
                    *vault,
                    vec![(length, router_count_attribute(vault)), (first, router_attribute(vault))],
                ));
            }
            out
        }

        /// Starting values of every attribute the package does not read from storage at
        /// creation: each provider's signer and approved executor, each vault's router (its
        /// provider), and the `balances` and `allowances` params.
        pub fn bootstrap_attributes(&self) -> Vec<(String, Vec<u8>)> {
            let mut out = Vec::new();
            for provider in &self.providers {
                out.push((signer_attribute(&provider.address), address_word(&provider.signer)));
                out.push((
                    approved_executor_attribute(&provider.address),
                    address_word(&self.executor),
                ));
                if let ProviderKind::Vault(vault) = provider.kind {
                    out.push((router_count_attribute(&vault), root(1).to_vec()));
                    out.push((router_attribute(&vault), address_word(&provider.address)));
                }
            }
            out.extend(
                self.bootstrap
                    .iter()
                    .map(|b| (b.attribute.clone(), b.value.clone())),
            );
            out
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
                        token.balance_slot(&provider.address),
                        balance_attribute(&token.address, &provider.address),
                    )),
                    ProviderKind::Vault(vault) => {
                        slots.push((
                            token.balance_slot(&vault),
                            balance_attribute(&token.address, &vault),
                        ));
                        slots.push((
                            token.allowance_slot(&vault, &provider.address),
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
        let (address, layout) = match parts.as_slice() {
            [address, "solady"] => (address, TokenLayout::Solady),
            [address, "b20"] => (address, TokenLayout::B20),
            [address, balance_slot, allowance_slot] => (
                address,
                TokenLayout::Mapping {
                    balance_slot: balance_slot.parse()?,
                    allowance_slot: allowance_slot.parse()?,
                },
            ),
            _ => {
                return Err(anyhow!(
                    "token `{entry}` is not address:balanceSlot:allowanceSlot, address:solady or \
                     address:b20"
                ))
            }
        };
        Ok(Token { address: parse_address(address)?, layout })
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
        Ok(Provider {
            address: parse_address(address)?,
            kind,
            signer: parse_address(signer)?,
            slots: None,
        })
    }

    fn parse_provider_slots(entry: &str) -> Result<(Address, ProviderSlots)> {
        let parts: Vec<&str> = entry.split(':').collect();
        let [address, signer, approved_executor] = parts.as_slice() else {
            return Err(anyhow!(
                "provider slots `{entry}` is not provider:signerSlot:approvedExecutorSlot"
            ));
        };
        Ok((
            parse_address(address)?,
            ProviderSlots {
                signer: signer.parse()?,
                approved_executor: approved_executor.parse()?,
            },
        ))
    }

    fn parse_vault_router(entry: &str) -> Result<(Address, u8)> {
        let (vault, slot) = entry
            .split_once(':')
            .ok_or_else(|| anyhow!("vault router `{entry}` is not vault:slot"))?;
        Ok((parse_address(vault)?, slot.parse()?))
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
             tokens=0x0000000000000000000000000000000000000003:3:4,0x0000000000000000000000000000000000000004:9:10,0x000000000000000000000000000000000000000c:solady,0x000000000000000000000000000000000000000d:b20&\
             providers=0x0000000000000000000000000000000000000005:balance:0x0000000000000000000000000000000000000006,\
             0x0000000000000000000000000000000000000007:vault:0x0000000000000000000000000000000000000008:0x0000000000000000000000000000000000000009&\
             provider_slots=0x0000000000000000000000000000000000000007:0:1&\
             vault_routers=0x0000000000000000000000000000000000000009:2&\
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
                Token {
                    address: addr(4),
                    layout: TokenLayout::Mapping { balance_slot: 9, allowance_slot: 10 }
                }
            );
            assert_eq!(config.tokens[2], Token { address: addr(12), layout: TokenLayout::Solady });
            assert_eq!(config.tokens[3], Token { address: addr(13), layout: TokenLayout::B20 });
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
            let provider = Provider {
                address: addr(7),
                kind: ProviderKind::Vault(addr(9)),
                signer: addr(8),
                slots: None,
            };
            let encoded = provider.encode();

            assert_eq!(encoded.len(), 21);
            assert_eq!(encoded[0], 1);
            assert_eq!(encoded[20], 9);
        }

        #[test]
        fn tracks_mutable_provider_slots_and_the_vault_router() {
            let config = Config::parse(PARAMS).unwrap();
            assert_eq!(config.providers[0].slots, None);
            assert_eq!(
                config.providers[1].slots,
                Some(ProviderSlots { signer: 0, approved_executor: 1 })
            );

            let slots = config.provider_state_slots();
            assert_eq!(slots.len(), 2);
            assert_eq!(slots[0].0, addr(7));
            assert_eq!(slots[0].1[1], (root(1), approved_executor_attribute(&addr(7))));
            // `router()` is the first element of the array at slot 2: keccak(uint256(2)).
            assert_eq!(slots[1].0, addr(9));
            assert_eq!(
                hex::encode(slots[1].1[1].0),
                "405787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace"
            );
        }

        #[test]
        fn bootstraps_signers_executor_and_vault_router() {
            let config = Config::parse(PARAMS).unwrap();
            let attributes = config.bootstrap_attributes();
            let value = |name: String| {
                attributes
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.clone())
                    .unwrap()
            };

            assert_eq!(value(signer_attribute(&addr(5))), address_word(&addr(6)));
            assert_eq!(value(approved_executor_attribute(&addr(7))), address_word(&addr(2)));
            assert_eq!(value(router_count_attribute(&addr(9))), root(1).to_vec());
            assert_eq!(value(router_attribute(&addr(9))), address_word(&addr(7)));
            // Two signers, two executors, one vault router (two words), two token values.
            assert_eq!(attributes.len(), 8);
        }

        #[test]
        fn rejects_slots_for_an_unlisted_provider() {
            let params = PARAMS.replace(
                "provider_slots=0x0000000000000000000000000000000000000007",
                "provider_slots=0x00000000000000000000000000000000000000ff",
            );
            assert!(Config::parse(&params).is_err());
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

        #[test]
        fn manifest_params_parse_with_every_token() {
            let manifest = include_str!("../../base-biconomy.yaml");
            let params: Vec<&str> = manifest
                .lines()
                .filter_map(|line| line.trim().split_once(": \""))
                .filter(|(key, _)| key.starts_with("map_"))
                .map(|(_, value)| value.trim_end_matches('"'))
                .collect();
            assert_eq!(params.len(), 3);
            for value in params {
                let config = Config::parse(value).unwrap();
                assert_eq!(config.tokens.len(), 18);
                assert_eq!(config.providers.len(), 2);
            }
        }

        fn addr(last: u8) -> Address {
            let mut a = [0u8; 20];
            a[19] = last;
            a
        }
    }
}

#[path = "5_map_protocol_changes.rs"]
mod map_protocol_changes;
#[path = "1_map_protocol_components.rs"]
mod map_protocol_components;
#[path = "3_map_state_writes.rs"]
mod map_state_writes;
#[path = "2_store_protocol_components.rs"]
mod store_protocol_components;
#[path = "4_store_state.rs"]
mod store_state;
