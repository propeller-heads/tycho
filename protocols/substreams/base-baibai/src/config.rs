use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{ensure, Result};
use substreams_ethereum::pb::eth::v2::{Block, TransactionTrace};

#[derive(serde::Deserialize)]
pub struct Config {
    pub entrypoint: Address,
    pub curve_book: Address,
    pub custodian: Address,
    pub base: Address,
    pub quote: Address,
    pub start_block: u64,
}

impl Config {
    pub fn parse(params: &str) -> Result<Self> {
        let config: Self = serde_qs::from_str(params)?;
        let addresses =
            [config.entrypoint, config.curve_book, config.custodian, config.base, config.quote];
        ensure!(
            addresses
                .iter()
                .all(|address| !address.is_zero()),
            "zero BaiBai address"
        );
        for (i, address) in addresses.iter().enumerate() {
            ensure!(!addresses[..i].contains(address), "duplicate BaiBai address");
        }
        Ok(config)
    }

    pub fn id(&self) -> String {
        format!("0x{:x}{:x}", self.entrypoint, self.base)
    }

    pub fn creation<'a>(&self, block: &'a Block) -> Result<&'a TransactionTrace> {
        block
            .transactions()
            .find(|tx| {
                tx.logs_with_calls()
                    .any(|(log, _)| log.address == self.entrypoint.as_slice())
            })
            .ok_or_else(|| anyhow::anyhow!("no entrypoint creation log in start block"))
    }

    /// CurveBook v3's ERC-7201 layout and the custodian's claim reservation mapping.
    /// The order is the simulator's word_0..word_31 wire format.
    pub fn slots(&self) -> Vec<(Address, B256)> {
        let book = namespace("baibai.storage.CurveBook.v3");
        let pair = mapping(U256::from_be_slice(self.base.as_slice()), book + U256::from(2));
        let mut slots = vec![(self.curve_book, B256::from(book))];
        for i in 0..5 {
            slots.push((self.curve_book, B256::from(pair + U256::from(i))));
        }
        for side in [5, 6] {
            for i in 0..12 {
                slots.push((
                    self.curve_book,
                    B256::from(mapping(U256::from(i), pair + U256::from(side))),
                ));
            }
        }
        let claims = namespace("baibai.storage.Custodian") + U256::from(2);
        for token in [self.base, self.quote] {
            slots.push((
                self.custodian,
                B256::from(mapping(U256::from_be_slice(token.as_slice()), claims)),
            ));
        }
        slots
    }
}

fn namespace(name: &str) -> U256 {
    let hash = U256::from_be_bytes(keccak256(name).0) - U256::from(1);
    U256::from_be_bytes(keccak256(hash.to_be_bytes::<32>()).0) & !U256::from(255)
}

fn mapping(key: U256, slot: U256) -> U256 {
    let mut encoded = [0u8; 64];
    encoded[..32].copy_from_slice(&key.to_be_bytes::<32>());
    encoded[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(encoded).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    pub fn config() -> Config {
        Config {
            entrypoint: address!("98c1d9e102eb2806d902b13186bdc7892ac4ffba"),
            curve_book: address!("604d9b9eb1e1571c78661a6c1088427ec9c8c6e5"),
            custodian: address!("aac48feb93c5c97e0fb3c7c57e1633922a4acda3"),
            base: address!("4200000000000000000000000000000000000006"),
            quote: address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            start_block: 50895895,
        }
    }

    #[test]
    fn storage_slots_match_fork_verified_layout() {
        let config = config();
        let slots = config.slots();
        assert_eq!(slots.len(), 32);
        assert_eq!(
            slots[0],
            (
                config.curve_book,
                b256!("e09504e49664366a3e335460a12239a33d2e4d6e11b3f715819aac7b9cbd4700")
            )
        );
        assert_eq!(
            slots[1].1,
            b256!("bcb8bfe6ffbb71dd8cc906f7ff382624d78bf34f08d0db2fe70575e6e31e7ccf")
        );
        assert_eq!(
            slots[6].1,
            b256!("39d4cddf50b0121ac262edb797550f5bb3d7a094920cc5f379ea8501da1526b9")
        );
        assert_eq!(
            slots[18].1,
            b256!("b06093be85115d3933e65e4d612df056dffaeeabd757f4892813dbf7d291f548")
        );
        assert_eq!(slots[30].0, config.custodian);
        assert_ne!(slots[30].1, slots[31].1);
        // IDs must round-trip through Tycho's byte representation and the SDK's ':' keys.
        let id = alloy_primitives::hex::decode(config.id()).unwrap();
        assert_eq!(id, [config.entrypoint.as_slice(), config.base.as_slice()].concat());
    }
}
