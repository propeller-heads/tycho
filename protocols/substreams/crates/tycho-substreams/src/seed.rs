//! Starting a package from an embedded seed.
//!
//! A seed is the state of the package's protocol after one block, written by the package's seed
//! writer (a native crate next to the package) in the `tycho-seed-format` layout: a header naming
//! the package and the block, then a package-specific protobuf body. The package embeds the file
//! with `include_bytes!` and, when it streams the seed block, emits the body as synthetic events
//! instead of the block's real events, so a manifest whose `initialBlock` is the seed block skips
//! the protocol's history. This module covers the protocol-free part of that: checking the header
//! and building the synthetic transaction. Turning the body into the package's events is the
//! package's own job.
//!
//! ```ignore
//! static SEED_BYTES: &[u8] = include_bytes!("../seed.bin");
//!
//! #[substreams::handlers::map]
//! fn map_events(block: eth::v2::Block) -> Result<BlockEvents, Error> {
//!     let seed = Seed::parse(SEED_BYTES, env!("CARGO_PKG_NAME"));
//!     if block.number == seed.header.block_number {
//!         seed.assert_block(&block);
//!         let tx = seed.transaction(&block, &CONTRACT);
//!         return Ok(events_from_seed_body(seed.body, tx));
//!     }
//!     // ...
//! }
//! ```

use substreams_ethereum::pb::eth::v2::Block;
pub use tycho_seed_format::Header;

use crate::models::Transaction;

/// A parsed seed file: the header and the package-specific body.
pub struct Seed<'a> {
    pub header: Header,
    pub body: &'a [u8],
}

impl<'a> Seed<'a> {
    /// Splits a seed file written for `package`.
    ///
    /// Panics when the bytes are empty, truncated, not a seed file or written for another
    /// package: a package built without a valid seed is a broken build, not an unseeded one.
    pub fn parse(bytes: &'a [u8], package: &str) -> Self {
        let (header, body) = tycho_seed_format::decode_for(bytes, package).unwrap_or_else(|e| {
            panic!("seed.bin: {e}; write one with the package's seed writer, e.g. its `empty` subcommand")
        });

        Self { header, body }
    }

    /// Panics unless `block` is the block the seed was taken from, so a seed can never be applied
    /// to another block or another chain.
    pub fn assert_block(&self, block: &Block) {
        assert_eq!(
            block.number, self.header.block_number,
            "seed was taken at block {} but is applied to block {}",
            self.header.block_number, block.number
        );
        assert_eq!(
            block.hash,
            self.header.block_hash,
            "seed was taken from block {} with hash 0x{} but the streamed block has hash 0x{}",
            self.header.block_number,
            hex::encode(self.header.block_hash),
            hex::encode(&block.hash)
        );
    }

    /// The synthetic transaction that carries the seeded state.
    ///
    /// Its hash is derived from the package and the block, `contract` is both sender and receiver,
    /// and its index follows the block's real transactions so the indexer never sees two
    /// transactions with the same index in one block.
    pub fn transaction(&self, block: &Block, contract: &[u8]) -> Transaction {
        Transaction {
            hash: self
                .header
                .genesis_transaction_hash()
                .to_vec(),
            from: contract.to_vec(),
            to: contract.to_vec(),
            index: block.transaction_traces.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::TransactionTrace;

    use super::*;

    const PACKAGE: &str = "ethereum-example";

    fn header() -> Header {
        Header { package: PACKAGE.to_owned(), block_number: 25_000_000, block_hash: [9; 32] }
    }

    fn block(hash: [u8; 32], tx_count: usize) -> Block {
        Block {
            number: 25_000_000,
            hash: hash.to_vec(),
            transaction_traces: vec![TransactionTrace::default(); tx_count],
            ..Default::default()
        }
    }

    #[test]
    fn parses_header_and_body() {
        let bytes = tycho_seed_format::encode(&header(), b"body").unwrap();

        let seed = Seed::parse(&bytes, PACKAGE);

        assert_eq!(seed.header, header());
        assert_eq!(seed.body, b"body");
    }

    #[test]
    #[should_panic(expected = "seed.bin: seed holds 0 bytes")]
    fn rejects_an_empty_file() {
        Seed::parse(&[], PACKAGE);
    }

    #[test]
    #[should_panic(expected = "not `ethereum-other`")]
    fn rejects_another_packages_seed() {
        let bytes = tycho_seed_format::encode(&header(), &[]).unwrap();

        Seed::parse(&bytes, "ethereum-other");
    }

    #[test]
    fn transaction_follows_the_real_ones() {
        let bytes = tycho_seed_format::encode(&header(), &[]).unwrap();
        let seed = Seed::parse(&bytes, PACKAGE);
        let block = block([9; 32], 3);

        seed.assert_block(&block);
        let tx = seed.transaction(&block, &[1; 20]);

        assert_eq!(
            tx.hash,
            header()
                .genesis_transaction_hash()
                .to_vec()
        );
        assert_eq!(tx.from, vec![1; 20]);
        assert_eq!(tx.to, vec![1; 20]);
        assert_eq!(tx.index, 3);
    }

    #[test]
    #[should_panic(expected = "but the streamed block has hash")]
    fn rejects_a_block_with_another_hash() {
        let bytes = tycho_seed_format::encode(&header(), &[]).unwrap();

        Seed::parse(&bytes, PACKAGE).assert_block(&block([8; 32], 0));
    }
}
