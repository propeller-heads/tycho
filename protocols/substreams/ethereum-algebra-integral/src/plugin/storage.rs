//! Fields inside storage words.

/// One value a plugin module keeps in storage, and the attribute it is emitted as.
///
/// The attribute's value is the field's bytes: big-endian and exactly as wide as the field, with
/// signed fields in two's complement at that width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageField {
    name: &'static str,
    slot: [u8; 32],
    offset: usize,
    width: usize,
}

impl StorageField {
    /// A field of `width` bytes, `offset` bytes above the low end of the word at `slot`, emitted
    /// as attribute `name`.
    ///
    /// # Panics
    ///
    /// At compile time, for a field that is empty or does not fit in a word.
    pub const fn new(name: &'static str, slot: [u8; 32], offset: usize, width: usize) -> Self {
        assert!(width > 0, "a storage field is at least one byte wide");
        assert!(offset + width <= 32, "a storage field ends inside its word");
        Self { name, slot, offset, width }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn slot(&self) -> &[u8; 32] {
        &self.slot
    }

    /// Bytes of the word this field covers, counted from the word's high end.
    pub(crate) const fn byte_range(&self) -> std::ops::Range<usize> {
        let end = 32 - self.offset;
        end - self.width..end
    }

    /// This field's bytes in `word`.
    pub(crate) fn read<'a>(&self, word: &'a [u8; 32]) -> &'a [u8] {
        &word[self.byte_range()]
    }
}

/// A storage value as a full word. Firehose may drop leading zero bytes.
pub(crate) fn word(value: &[u8]) -> [u8; 32] {
    let mut word = [0u8; 32];
    let value = &value[value.len().saturating_sub(32)..];
    word[32 - value.len()..].copy_from_slice(value);
    word
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;

    const SLOT: [u8; 32] = [0u8; 32];
    const WORD: [u8; 32] = hex!("ff0000000000000000000000000001f400010001000000000000000000000abc");

    #[rstest]
    #[case::low_end(0, 2, &hex!("0abc"))]
    #[case::middle(16, 2, &hex!("01f4"))]
    #[case::high_end(31, 1, &hex!("ff"))]
    #[case::whole_word(0, 32, &WORD)]
    fn read_returns_the_fields_bytes(
        #[case] offset: usize,
        #[case] width: usize,
        #[case] expected: &[u8],
    ) {
        let field = StorageField::new("field", SLOT, offset, width);

        assert_eq!(field.read(&WORD), expected);
    }

    #[rstest]
    #[case::full(&WORD, WORD)]
    #[case::short(&hex!("01f4"), hex!("00000000000000000000000000000000000000000000000000000000000001f4"))]
    #[case::empty(&[], [0u8; 32])]
    fn word_pads_on_the_high_end(#[case] value: &[u8], #[case] expected: [u8; 32]) {
        assert_eq!(word(value), expected);
    }

    #[test]
    fn word_keeps_the_low_32_bytes_of_a_longer_value() {
        let mut value = vec![0xee];
        value.extend_from_slice(&WORD);

        assert_eq!(word(&value), WORD);
    }
}
