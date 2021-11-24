//! Hummock SST builder.
//!
//! The SST format is exactly the same as `AgateDB` (`BadgerDB`), and is very similar to `RocksDB`.

// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use super::utils::bytes_diff;
use crate::storage::hummock::bloom::Bloom;
use crate::storage::hummock::format::user_key;
use crate::storage::hummock::table::utils::crc32_checksum;
use crate::storage::hummock::HummockValue;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message;
use risingwave_pb::hummock::checksum::Algorithm as ChecksumAlg;
use risingwave_pb::hummock::{BlockOffset, Checksum, TableMeta};

/// Entry header stores the difference between current key and block base key. `overlap` is the
/// common prefix of key and base key, and diff is the length of different part.
#[derive(Default)]
pub struct Header {
    /// Overlap with base key.
    pub overlap: u16,

    /// Length of the diff.
    pub diff: u16,
}

pub const HEADER_SIZE: usize = std::mem::size_of::<Header>();

impl Header {
    /// Encode encodes the header.
    pub fn encode(&self, bytes: &mut impl BufMut) {
        bytes.put_u32_le((self.overlap as u32) << 16 | self.diff as u32);
    }

    /// Decode decodes the header.
    pub fn decode(&mut self, bytes: &mut impl Buf) {
        let h = bytes.get_u32_le();
        self.overlap = (h >> 16) as u16;
        self.diff = h as u16;
    }
}

#[derive(Debug, Clone)]
pub struct TableBuilderOptions {
    /// Target capacity of the table
    pub table_capacity: u32,

    /// Size of each block in bytes in SST
    pub block_size: u32,

    /// False positive probability of Bloom filter
    pub bloom_false_positive: f64,
}

/// Builder is used in building a table.
/// Builder builds an SST that consists of two parts:
/// - Table data is simply a sequence of blocks.
/// - Metadata is the prost-encoded `TableMeta` data and essential information to determine the
/// checksum.
pub struct TableBuilder {
    options: TableBuilderOptions,

    meta: TableMeta,

    /// Buffer blocks data
    data_buf: BytesMut,

    /// Used for prefix-encode
    base_key: Bytes,
    base_offset: u32,

    /// Entry offsets in a block
    entry_offsets: Vec<u32>,

    /// Used for building the Bloom filter
    key_hashes: Vec<u32>,
}

impl TableBuilder {
    /// Create new builder from options
    pub fn new(options: TableBuilderOptions) -> Self {
        Self {
            // approximately 16MB index + table size
            data_buf: BytesMut::with_capacity(options.table_capacity as usize),
            meta: TableMeta::default(),
            base_key: Bytes::new(),
            base_offset: 0,
            key_hashes: Vec::with_capacity(1024),
            entry_offsets: vec![],
            options,
        }
    }

    /// Check if the builder is empty
    pub fn is_empty(&self) -> bool {
        self.data_buf.is_empty()
    }

    /// Calculate the difference of two keys
    fn key_diff<'a>(&self, key: &'a [u8]) -> &'a [u8] {
        bytes_diff(&self.base_key, key)
    }

    /// Append encoded block bytes to the buffer
    fn finish_block(&mut self) {
        // ---------- encode block ----------

        // different behavior: BadgerDB will just return.
        assert!(!self.entry_offsets.is_empty());

        // encode offsets list and its length
        for offset in &self.entry_offsets {
            self.data_buf.put_u32_le(*offset);
        }
        self.data_buf.put_u32(self.entry_offsets.len() as u32);

        // encode checksum and its length
        let checksum = Checksum {
            sum: crc32_checksum(&self.data_buf[self.base_offset as usize..]),
            algo: ChecksumAlg::Crc32c as i32,
        };
        let mut cs_bytes = BytesMut::new();
        checksum.encode(&mut cs_bytes).unwrap();
        let ck_len = cs_bytes.len() as u32;
        self.data_buf.put(cs_bytes);
        self.data_buf.put_u32(ck_len);

        // ---------- add block offset to meta ----------
        let block_offset = BlockOffset {
            key: self.base_key.to_vec(),
            offset: self.base_offset,
            len: self.data_buf.len() as u32 - self.base_offset,
        };
        self.meta.offsets.push(block_offset);
    }

    fn should_finish_block(&self, key: &[u8], value: &HummockValue<Vec<u8>>) -> bool {
        // If there is no entry till now, we will return false.
        if self.entry_offsets.is_empty() {
            return false;
        }

        // We should include current entry also in size, that's why +1 to len(b.entryOffsets).
        let entries_offsets_size = ((self.entry_offsets.len() + 1) * 4 +
        4 + // size of list
        8 + // sum64 in checksum proto
        4) as u32; // checksum length
                   // Integer overflow check for statements above.
        assert!(entries_offsets_size < u32::MAX);
        let estimated_size = (self.data_buf.len() as u32)
            - self.base_offset + 6 // header size for entry
            + key.len() as u32
            + value.encoded_len() as u32
            + entries_offsets_size;

        // Integer overflow check for table size.
        assert!(self.data_buf.len() as u32 + estimated_size < u32::MAX);

        estimated_size > self.options.block_size
    }

    /// Table data format:
    /// ```plain
    /// | Block | Block | Block | Block | Block |
    /// ```
    /// Add adds a key-value pair to the block.
    pub fn add(&mut self, key: &[u8], value: HummockValue<Vec<u8>>) {
        if self.should_finish_block(key, &value) {
            self.finish_block();
            self.base_key.clear();
            assert!(self.data_buf.len() < u32::MAX as usize);
            self.base_offset = self.data_buf.len() as u32;
            self.entry_offsets.clear();
        }

        self.key_hashes.push(farmhash::fingerprint32(user_key(key)));

        // diff_key stores the difference of key with baseKey.
        let diff_key = if self.base_key.is_empty() {
            self.base_key = key.to_vec().into();
            key
        } else {
            self.key_diff(key)
        };
        assert!(key.len() - diff_key.len() <= u16::MAX as usize);
        assert!(diff_key.len() <= u16::MAX as usize);

        // get header
        let header = Header {
            overlap: (key.len() - diff_key.len()) as u16,
            diff: diff_key.len() as u16,
        };
        assert!(self.data_buf.len() <= u32::MAX as usize);

        // store current entry's offset
        self.entry_offsets
            .push(self.data_buf.len() as u32 - self.base_offset);

        // entry layout: header, diffKey, value.
        header.encode(&mut self.data_buf);
        self.data_buf.put_slice(diff_key);
        value.encode(&mut self.data_buf);

        // update estimated size
        let block_size = value.encoded_len() + diff_key.len() + 4;
        self.meta.estimated_size += block_size as u32;
    }

    /// Returns true if we roughly reached capacity
    pub fn reach_capacity(&self) -> bool {
        let block_size = self.data_buf.len() as u32 + // actual length of current buffer
                                 self.entry_offsets.len() as u32 * 4 + // all entry offsets size
                                 4 + // count of all entry offsets
                                 8 + // checksum bytes
                                 4; // checksum length

        let estimated_size = block_size +
                                  4 + // index length
                                  5 * self.meta.offsets.len() as u32; // TODO: why 5?
        estimated_size as u32 > self.options.table_capacity
    }

    /// Finalize the table to be blocks and metadata
    pub fn finish(mut self) -> (Bytes, TableMeta) {
        // Append blocks. This will never start a new block.
        self.finish_block();

        // TODO: move boundaries and build index if we need to encrypt or compress

        // initial Bloom filter
        if self.options.bloom_false_positive > 0.0 {
            let bits_per_key =
                Bloom::bloom_bits_per_key(self.key_hashes.len(), self.options.bloom_false_positive);
            let bloom = Bloom::build_from_key_hashes(&self.key_hashes, bits_per_key);
            self.meta.bloom_filter = bloom.to_vec();
        }

        (self.data_buf.freeze(), self.meta)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::storage::hummock::Table;
    use itertools::Itertools;

    const TEST_KEYS_COUNT: usize = 100000;

    #[test]
    #[should_panic]
    fn test_empty() {
        let opt = TableBuilderOptions {
            bloom_false_positive: 0.1,
            block_size: 0,
            table_capacity: 0,
        };

        let b = TableBuilder::new(opt);

        b.finish();
    }

    #[test]
    fn test_header_encode_decode() {
        let mut header = Header {
            overlap: 23333,
            diff: 23334,
        };
        let mut buf = BytesMut::new();
        header.encode(&mut buf);
        let mut buf = buf.freeze();
        header.decode(&mut buf);
        assert_eq!(header.overlap, 23333);
        assert_eq!(header.diff, 23334);
    }

    pub fn generate_table() -> (Bytes, TableMeta) {
        let opt = TableBuilderOptions {
            bloom_false_positive: 0.0,
            block_size: 0,
            table_capacity: 0,
        };

        let mut b = TableBuilder::new(opt);

        for i in 0..10000 {
            b.add(
                format!("key_test_{}", i).as_bytes(),
                HummockValue::Put(
                    "23332333"
                        .as_bytes()
                        .iter()
                        .cycle()
                        .cloned()
                        .take(i + 1)
                        .collect_vec(),
                ),
            );
        }

        b.finish()
    }

    fn key(prefix: &[u8], i: usize) -> Bytes {
        Bytes::from([prefix, format!("{:04}", i).as_bytes()].concat())
    }

    fn test_with_bloom_filter(with_blooms: bool) {
        let key_count = 1000;

        let opt = TableBuilderOptions {
            bloom_false_positive: if with_blooms { 0.01 } else { 0.0 },
            block_size: 0,
            table_capacity: 0,
        };

        let mut b = TableBuilder::new(opt);

        for i in 0..key_count {
            b.add(
                format!("key_test_{}", i).as_bytes(),
                HummockValue::Put(
                    "23332333"
                        .as_bytes()
                        .iter()
                        .cycle()
                        .cloned()
                        .take(i + 1)
                        .collect_vec(),
                ),
            );
        }

        let (blocks, meta) = b.finish();
        let table = Table::load(0, blocks, meta).unwrap();
        assert_eq!(table.has_bloom_filter, with_blooms);
        for i in 0..key_count {
            let hash = farmhash::fingerprint32(user_key(format!("key_test_{}", i).as_bytes()));
            assert!(!table.surely_not_have(hash));
        }
    }

    #[test]
    fn test_bloom_filter() {
        test_with_bloom_filter(false);
        test_with_bloom_filter(true);
    }

    #[test]
    fn test_build() {
        generate_table();
    }
}
