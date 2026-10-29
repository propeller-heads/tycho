use std::{
    borrow::Borrow,
    clone::Clone,
    fmt::{Debug, Display, Formatter, LowerHex, Result as FmtResult},
    ops::Deref,
    str::FromStr,
};

use deepsize::{Context, DeepSizeOf};
#[cfg(feature = "diesel")]
use diesel::{
    deserialize::{self, FromSql, FromSqlRow},
    expression::AsExpression,
    pg::Pg,
    serialize::{self, ToSql},
    sql_types::Binary,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::serde_primitives::hex_bytes;

/// Wrapper type around Bytes to deserialize/serialize from/to hex
#[derive(Clone, Default, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[cfg_attr(feature = "diesel", derive(AsExpression, FromSqlRow,))]
#[cfg_attr(feature = "diesel", diesel(sql_type = Binary))]
pub struct Bytes(#[serde(with = "hex_bytes")] pub bytes::Bytes);

impl DeepSizeOf for Bytes {
    fn deep_size_of_children(&self, _ctx: &mut Context) -> usize {
        // Note: This may overcount memory if the underlying bytes are shared (e.g. via Arc).
        // We cannot detect shared ownership here because Context’s internal tracking is private
        // At the same time, this might also underreport memory if:
        // - the bytes::Bytes are instantiated as Shared internally, which adds 24 bytes of overhead
        // - the bytes::Bytes has capacity greater than its length, as we only count the length here
        self.0.len()
    }
}

fn bytes_to_hex(b: &Bytes) -> String {
    hex::encode(b.0.as_ref())
}

impl Debug for Bytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "Bytes(0x{})", bytes_to_hex(self))
    }
}

impl Display for Bytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "0x{}", bytes_to_hex(self))
    }
}

impl LowerHex for Bytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "0x{}", bytes_to_hex(self))
    }
}

impl Bytes {
    pub fn new() -> Self {
        Self(bytes::Bytes::new())
    }
    /// This function converts the internal byte array into a `Vec<u8>`
    ///
    /// # Returns
    ///
    /// A `Vec<u8>` containing the bytes from the `Bytes` struct.
    ///
    /// # Example
    ///
    /// ```
    /// let bytes = Bytes::from(vec![0x01, 0x02, 0x03]);
    /// let vec = bytes.to_vec();
    /// assert_eq!(vec, vec![0x01, 0x02, 0x03]);
    /// ```
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_ref().to_vec()
    }

    /// Left-pads the byte array to the specified length with the given padding byte.
    ///
    /// This function creates a new `Bytes` instance by prepending the specified padding byte
    /// to the current byte array until its total length matches the desired length.
    ///
    /// If the current length of the byte array is greater than or equal to the specified length,
    /// the original byte array is returned unchanged.
    ///
    /// # Arguments
    ///
    /// * `length` - The desired total length of the resulting byte array.
    /// * `pad_byte` - The byte value to use for padding. Commonly `0x00`.
    ///
    /// # Returns
    ///
    /// A new `Bytes` instance with the byte array left-padded to the desired length.
    ///
    /// # Example
    ///
    /// ```
    /// let bytes = Bytes::from(vec![0x01, 0x02, 0x03]);
    /// let padded = bytes.lpad(6, 0x00);
    /// assert_eq!(padded.to_vec(), vec![0x00, 0x00, 0x00, 0x01, 0x02, 0x03]);
    /// ```
    pub fn lpad(&self, length: usize, pad_byte: u8) -> Bytes {
        let mut padded_vec = vec![pad_byte; length.saturating_sub(self.len())];
        padded_vec.extend_from_slice(self.as_ref());

        Bytes(bytes::Bytes::from(padded_vec))
    }

    /// Right-pads the byte array to the specified length with the given padding byte.
    ///
    /// This function creates a new `Bytes` instance by appending the specified padding byte
    /// to the current byte array until its total length matches the desired length.
    ///
    /// If the current length of the byte array is greater than or equal to the specified length,
    /// the original byte array is returned unchanged.
    ///
    /// # Arguments
    ///
    /// * `length` - The desired total length of the resulting byte array.
    /// * `pad_byte` - The byte value to use for padding. Commonly `0x00`.
    ///
    /// # Returns
    ///
    /// A new `Bytes` instance with the byte array right-padded to the desired length.
    ///
    /// # Example
    ///
    /// ```
    /// let bytes = Bytes::from(vec![0x01, 0x02, 0x03]);
    /// let padded = bytes.rpad(6, 0x00);
    /// assert_eq!(padded.to_vec(), vec![0x01, 0x02, 0x03, 0x00, 0x00, 0x00]);
    /// ```
    pub fn rpad(&self, length: usize, pad_byte: u8) -> Bytes {
        let mut padded_vec = self.to_vec();
        padded_vec.resize(length, pad_byte);

        Bytes(bytes::Bytes::from(padded_vec))
    }

    /// Creates a `Bytes` object of the specified length, filled with zeros.
    ///
    /// # Arguments
    ///
    /// * `length` - The length of the `Bytes` object to be created.
    ///
    /// # Returns
    ///
    /// A `Bytes` object of the specified length, where each byte is set to zero.
    ///
    /// # Example
    ///
    /// ```
    /// let b = Bytes::zero(5);
    /// assert_eq!(b, Bytes::from(vec![0, 0, 0, 0, 0]));
    /// ```
    pub fn zero(length: usize) -> Bytes {
        Bytes::from(vec![0u8; length])
    }

    /// Creates a `Bytes` object of the specified length, filled with random bytes.
    ///
    /// # Arguments
    ///
    /// * `length` - The length of the `Bytes` object to be created.
    ///
    /// # Returns
    ///
    /// A `Bytes` object of the specified length, filled with random bytes.
    ///
    /// # Example
    ///
    /// ```
    /// let random_bytes = Bytes::random(5);
    /// assert_eq!(random_bytes.len(), 5);
    /// ```
    pub fn random(length: usize) -> Bytes {
        let mut data = vec![0u8; length];
        rand::thread_rng().fill(&mut data[..]);
        Bytes::from(data)
    }

    /// Checks if the byte array is full of zeros.
    ///
    /// # Returns
    ///
    /// A boolean value indicating whether all bytes in the byte array are zero.
    ///
    /// # Example
    ///
    /// ```
    /// let b = Bytes::zero(5);
    /// assert!(b.is_zero());
    /// ```
    pub fn is_zero(&self) -> bool {
        self.as_ref().iter().all(|b| *b == 0)
    }
}

impl Deref for Bytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl IntoIterator for Bytes {
    type Item = u8;
    type IntoIter = bytes::buf::IntoIter<bytes::Bytes>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Bytes {
    type Item = &'a u8;
    type IntoIter = core::slice::Iter<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_ref().iter()
    }
}

impl From<&[u8]> for Bytes {
    fn from(src: &[u8]) -> Self {
        Self(bytes::Bytes::copy_from_slice(src))
    }
}

impl From<bytes::Bytes> for Bytes {
    fn from(src: bytes::Bytes) -> Self {
        Self(src)
    }
}

impl From<Bytes> for bytes::Bytes {
    fn from(src: Bytes) -> Self {
        src.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(src: Vec<u8>) -> Self {
        Self(src.into())
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(value: Bytes) -> Self {
        value.to_vec()
    }
}

impl<const N: usize> From<[u8; N]> for Bytes {
    fn from(src: [u8; N]) -> Self {
        src.to_vec().into()
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for Bytes {
    fn from(src: &'a [u8; N]) -> Self {
        src.to_vec().into()
    }
}

impl PartialEq<[u8]> for Bytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}

impl PartialEq<Bytes> for [u8] {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialEq<Vec<u8>> for Bytes {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.as_ref() == &other[..]
    }
}

impl PartialEq<Bytes> for Vec<u8> {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialEq<bytes::Bytes> for Bytes {
    fn eq(&self, other: &bytes::Bytes) -> bool {
        other == self.as_ref()
    }
}

#[derive(Debug, Clone, Error)]
#[error("Failed to parse bytes: {0}")]
pub struct ParseBytesError(String);

impl FromStr for Bytes {
    type Err = ParseBytesError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(value) = value.strip_prefix("0x") {
            hex::decode(value)
        } else {
            hex::decode(value)
        }
        .map(Into::into)
        .map_err(|e| ParseBytesError(format!("Invalid hex: {e}")))
    }
}

impl From<&str> for Bytes {
    fn from(value: &str) -> Self {
        value.parse().unwrap()
    }
}

#[cfg(feature = "diesel")]
impl ToSql<Binary, Pg> for Bytes {
    fn to_sql<'b>(&'b self, out: &mut serialize::Output<'b, '_, Pg>) -> serialize::Result {
        let bytes_slice: &[u8] = &self.0;
        <&[u8] as ToSql<Binary, Pg>>::to_sql(&bytes_slice, &mut out.reborrow())
    }
}

#[cfg(feature = "diesel")]
impl FromSql<Binary, Pg> for Bytes {
    fn from_sql(
        bytes: <diesel::pg::Pg as diesel::backend::Backend>::RawValue<'_>,
    ) -> deserialize::Result<Self> {
        let byte_vec: Vec<u8> = <Vec<u8> as FromSql<Binary, Pg>>::from_sql(bytes)?;
        Ok(Bytes(bytes::Bytes::from(byte_vec)))
    }
}

macro_rules! impl_from_uint_for_bytes {
    ($($t:ty),*) => {
        $(
            impl From<$t> for Bytes {
                fn from(src: $t) -> Self {
                    let size = std::mem::size_of::<$t>();
                    let mut buf = vec![0u8; size];
                    buf.copy_from_slice(&src.to_be_bytes());

                    Self(bytes::Bytes::from(buf))
                }
            }
        )*
    };
}

impl_from_uint_for_bytes!(u8, u16, u32, u64, u128);

/// A big-endian byte string holds a value outside the integer type it was converted into.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{value} ({len} bytes) does not fit into {target}: at most {max} significant bytes")]
pub struct IntegerFromBytesError {
    value: Bytes,
    len: usize,
    target: &'static str,
    max: usize,
}

impl IntegerFromBytesError {
    fn new(value: Bytes, target: &'static str, max: usize) -> Self {
        let len = value.len();
        Self { value, len, target, max }
    }
}

/// Drops the leading bytes that carry no information: zero bytes for an unsigned value, and for a
/// signed one the sign-extension bytes (`0x00` before a byte with a clear sign bit, `0xFF` before a
/// byte with a set sign bit).
fn significant_bytes(bytes: &[u8], signed: bool) -> &[u8] {
    let Some(&first) = bytes.first() else { return bytes };
    let fill = if signed && first & 0x80 != 0 { 0xFF } else { 0x00 };
    let mut start = 0;
    while start + 1 < bytes.len() && bytes[start] == fill {
        let next_sign_matches = !signed || (bytes[start + 1] & 0x80) == (fill & 0x80);
        if !next_sign_matches {
            break;
        }
        start += 1;
    }
    &bytes[start..]
}

macro_rules! impl_try_from_bytes_for_int {
    ($signed:literal, $($t:ty),*) => {
        $(
            impl TryFrom<Bytes> for $t {
                type Error = IntegerFromBytesError;

                /// Interprets the bytes as a big-endian integer, sign-extending for signed
                /// types. Fails when the value does not fit the target type.
                fn try_from(src: Bytes) -> Result<Self, Self::Error> {
                    const SIZE: usize = std::mem::size_of::<$t>();
                    let digits = significant_bytes(src.as_ref(), $signed);
                    let Some(pad) = SIZE.checked_sub(digits.len()) else {
                        return Err(IntegerFromBytesError::new(src, stringify!($t), SIZE));
                    };
                    let negative = $signed && digits.first().is_some_and(|&b| b & 0x80 != 0);
                    let mut buf = if negative { [0xFFu8; SIZE] } else { [0x00u8; SIZE] };
                    buf[pad..].copy_from_slice(digits);
                    Ok(<$t>::from_be_bytes(buf))
                }
            }
        )*
    };
}

impl_try_from_bytes_for_int!(false, u8, u16, u32, u64, u128);
impl_try_from_bytes_for_int!(true, i8, i16, i32, i64, i128);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_bytes() {
        let b = bytes::Bytes::from("0123456789abcdef");
        let wrapped_b = Bytes::from(b.clone());
        let expected = Bytes(b);

        assert_eq!(wrapped_b, expected);
    }

    #[test]
    fn test_from_slice() {
        let arr = [1, 35, 69, 103, 137, 171, 205, 239];
        let b = Bytes::from(&arr);
        let expected = Bytes(bytes::Bytes::from(arr.to_vec()));

        assert_eq!(b, expected);
    }

    #[test]
    fn hex_formatting() {
        let b = Bytes::from(vec![1, 35, 69, 103, 137, 171, 205, 239]);
        let expected = String::from("0x0123456789abcdef");
        assert_eq!(format!("{b:x}"), expected);
        assert_eq!(format!("{b}"), expected);
    }

    #[test]
    fn test_from_str() {
        let b = Bytes::from_str("0x1213");
        assert!(b.is_ok());
        let b = b.unwrap();
        assert_eq!(b.as_ref(), hex::decode("1213").unwrap());

        let b = Bytes::from_str("1213");
        let b = b.unwrap();
        assert_eq!(b.as_ref(), hex::decode("1213").unwrap());
    }

    #[test]
    fn test_debug_formatting() {
        let b = Bytes::from(vec![1, 35, 69, 103, 137, 171, 205, 239]);
        assert_eq!(format!("{b:?}"), "Bytes(0x0123456789abcdef)");
        assert_eq!(format!("{b:#?}"), "Bytes(0x0123456789abcdef)");
    }

    #[test]
    fn test_to_vec() {
        let vec = vec![1, 35, 69, 103, 137, 171, 205, 239];
        let b = Bytes::from(vec.clone());

        assert_eq!(b.to_vec(), vec);
    }

    #[test]
    fn test_vec_partialeq() {
        let vec = vec![1, 35, 69, 103, 137, 171, 205, 239];
        let b = Bytes::from(vec.clone());
        assert_eq!(b, vec);
        assert_eq!(vec, b);

        let wrong_vec = vec![1, 3, 52, 137];
        assert_ne!(b, wrong_vec);
        assert_ne!(wrong_vec, b);
    }

    #[test]
    fn test_bytes_partialeq() {
        let b = bytes::Bytes::from("0123456789abcdef");
        let wrapped_b = Bytes::from(b.clone());
        assert_eq!(wrapped_b, b);

        let wrong_b = bytes::Bytes::from("0123absd");
        assert_ne!(wrong_b, b);
    }

    #[test]
    fn test_u128_from_bytes() {
        let data = Bytes::from(vec![4, 3, 2, 1]);
        let result = u128::try_from(data).unwrap();
        assert_eq!(result, u128::from_str("67305985").unwrap());
    }

    #[test]
    fn test_i128_from_bytes() {
        let data = Bytes::from(vec![4, 3, 2, 1]);
        let result = i128::try_from(data).unwrap();
        assert_eq!(result, i128::from_str("67305985").unwrap());
    }

    #[test]
    fn test_i32_from_bytes() {
        let data = Bytes::from(vec![4, 3, 2, 1]);
        let result = i32::try_from(data).unwrap();
        assert_eq!(result, i32::from_str("67305985").unwrap());
    }
}

#[cfg(feature = "diesel")]
#[cfg(test)]
mod diesel_tests {
    use diesel::{insert_into, table, Insertable, Queryable};
    use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

    use super::*;

    async fn setup_db() -> AsyncPgConnection {
        let db_url = std::env::var("DATABASE_URL").unwrap();
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .unwrap();
        conn.begin_test_transaction()
            .await
            .unwrap();
        conn
    }

    #[tokio::test]
    async fn test_bytes_db_round_trip() {
        table! {
            bytes_table (id) {
                id -> Int4,
                data -> Binary,
            }
        }

        #[derive(Insertable)]
        #[diesel(table_name = bytes_table)]
        struct NewByteEntry {
            data: Bytes,
        }

        #[derive(Queryable, PartialEq)]
        struct ByteEntry {
            id: i32,
            data: Bytes,
        }

        let mut conn = setup_db().await;
        let example_bytes = Bytes::from_str("0x0123456789abcdef").unwrap();

        conn.batch_execute(
            r"
            CREATE TEMPORARY TABLE bytes_table (
                id SERIAL PRIMARY KEY,
                data BYTEA NOT NULL
            );
        ",
        )
        .await
        .unwrap();

        let new_entry = NewByteEntry { data: example_bytes.clone() };

        let inserted: Vec<ByteEntry> = insert_into(bytes_table::table)
            .values(&new_entry)
            .get_results(&mut conn)
            .await
            .unwrap();

        assert_eq!(inserted[0].data, example_bytes);
    }
}
