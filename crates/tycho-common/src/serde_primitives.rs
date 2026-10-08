use std::{collections::HashMap, fmt, hash::Hash, marker::PhantomData};

use hex::FromHexError;
use serde::{de, Deserialize, Deserializer};

use crate::Bytes;

/// Decodes a hex string with an optional `0x` prefix. An odd number of digits has an implied
/// leading zero.
fn decode_hex_with_prefix(val: &str) -> Result<Vec<u8>, FromHexError> {
    let digits = val
        .strip_prefix("0x")
        .unwrap_or(val)
        .as_bytes();
    let mut out = vec![0u8; digits.len().div_ceil(2)];
    if digits.len().is_multiple_of(2) {
        hex::decode_to_slice(digits, &mut out)?;
    } else if let (Some((first, rest)), Some((first_out, rest_out))) =
        (digits.split_first(), out.split_first_mut())
    {
        hex::decode_to_slice([b'0', *first], std::slice::from_mut(first_out))?;
        hex::decode_to_slice(rest, rest_out)?;
    }
    Ok(out)
}

/// Bytes decoded from a hex string. The string is decoded where the deserializer holds it, without
/// copying it into a `String` first.
struct HexValue(Vec<u8>);

impl<'de> Deserialize<'de> for HexValue {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HexVisitor;

        impl de::Visitor<'_> for HexVisitor {
            type Value = HexValue;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a hex string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<HexValue, E> {
                decode_hex_with_prefix(value)
                    .map(HexValue)
                    .map_err(|e| E::custom(e.to_string()))
            }
        }

        d.deserialize_str(HexVisitor)
    }
}

impl From<HexValue> for Bytes {
    fn from(value: HexValue) -> Self {
        Bytes::from(value.0)
    }
}

/// Deserializes a map straight into its final types: each entry is read as `(KR, VR)` and
/// converted, with no intermediate map.
fn deserialize_map<'de, D, KR, VR, K, V>(d: D) -> Result<HashMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    KR: Deserialize<'de> + Into<K>,
    VR: Deserialize<'de> + Into<V>,
    K: Eq + Hash,
{
    struct MapVisitor<KR, VR, K, V>(PhantomData<(KR, VR, K, V)>);

    impl<'de, KR, VR, K, V> de::Visitor<'de> for MapVisitor<KR, VR, K, V>
    where
        KR: Deserialize<'de> + Into<K>,
        VR: Deserialize<'de> + Into<V>,
        K: Eq + Hash,
    {
        type Value = HashMap<K, V>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a map")
        }

        fn visit_map<A: de::MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut map = HashMap::with_capacity(access.size_hint().unwrap_or(0));
            while let Some((key, value)) = access.next_entry::<KR, VR>()? {
                map.insert(key.into(), value.into());
            }
            Ok(map)
        }
    }

    d.deserialize_map(MapVisitor::<KR, VR, K, V>(PhantomData))
}

/// A buffer for writing bytes as `0x`-prefixed lowercase hex.
///
/// Reusing one buffer for many values allocates only when a value is longer than every value
/// before it.
#[derive(Default)]
struct HexBuffer(Vec<u8>);

impl HexBuffer {
    /// Returns `bytes` as `0x` followed by two lowercase hex digits per byte. Empty input gives
    /// `"0x"`. The string borrows the buffer and is valid until the next call.
    ///
    /// # Errors
    ///
    /// Returns a serializer error if the hex string length overflows `usize`.
    fn encode<E: serde::ser::Error>(&mut self, bytes: &[u8]) -> Result<&str, E> {
        let hex_len = bytes
            .len()
            .checked_mul(2)
            .and_then(|digits| digits.checked_add(2))
            .ok_or_else(|| {
                E::custom(format!("cannot hex encode {} bytes: length overflows", bytes.len()))
            })?;
        self.0.clear();
        self.0.extend_from_slice(b"0x");
        self.0.resize(hex_len, 0);
        let digits = self
            .0
            .get_mut(2..)
            .ok_or_else(|| E::custom("hex buffer is shorter than its prefix"))?;
        hex::encode_to_slice(bytes, digits).map_err(E::custom)?;
        std::str::from_utf8(&self.0).map_err(E::custom)
    }
}

/// serde functions for handling bytes as hex strings, such as [bytes::Bytes]
pub mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{HexBuffer, HexValue};

    /// Serialize a byte vec as a hex string with 0x prefix
    pub fn serialize<S, T>(x: T, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: AsRef<[u8]>,
    {
        s.serialize_str(HexBuffer::default().encode(x.as_ref())?)
    }

    /// Deserialize a hex string into a byte vec
    /// Accepts a hex string with optional 0x prefix
    pub fn deserialize<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: From<Vec<u8>>,
    {
        HexValue::deserialize(d).map(|value| value.0.into())
    }
}

/// serde functions for handling Option of bytes
pub mod hex_bytes_option {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{HexBuffer, HexValue};

    /// Serialize a byte vec as a Some hex string with 0x prefix
    pub fn serialize<S, T>(x: &Option<T>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: AsRef<[u8]>,
    {
        if let Some(x) = x {
            s.serialize_str(HexBuffer::default().encode(x.as_ref())?)
        } else {
            s.serialize_none()
        }
    }

    /// Deserialize a hex string into a byte vec or None
    /// Accepts a hex string with optional 0x prefix
    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: From<Vec<u8>>,
    {
        let value: Option<HexValue> = Option::deserialize(d)?;
        Ok(value.map(|value| value.0.into()))
    }
}

/// serde functions for handling HashMap with a bytes key
pub mod hex_hashmap_key {
    use std::collections::HashMap;

    use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

    use super::{deserialize_map, HexBuffer, HexValue};
    use crate::Bytes;

    pub fn serialize<S, V>(x: &HashMap<Bytes, V>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        V: Serialize,
    {
        let mut key_hex = HexBuffer::default();
        let mut map = s.serialize_map(Some(x.len()))?;
        for (k, v) in x.iter() {
            map.serialize_entry(key_hex.encode(k)?, v)?;
        }
        map.end()
    }

    pub fn deserialize<'de, V, D>(d: D) -> Result<HashMap<Bytes, V>, D::Error>
    where
        D: Deserializer<'de>,
        V: Deserialize<'de>,
    {
        deserialize_map::<D, HexValue, V, Bytes, V>(d)
    }
}

/// serde functions for handling Vec of Bytes as hex strings
pub mod hex_bytes_vec {
    use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serializer};

    use super::{HexBuffer, HexValue};

    /// Serialize Vec<Vec<u8>> as a list of hex strings with 0x prefix
    pub fn serialize<S>(list: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut hex = HexBuffer::default();
        let mut seq = s.serialize_seq(Some(list.len()))?;
        for x in list {
            seq.serialize_element(hex.encode(x)?)?;
        }
        seq.end()
    }

    /// Deserialize a list of hex strings into Vec<Vec<u8>>
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<HexValue>::deserialize(d)?;
        Ok(values
            .into_iter()
            .map(|value| value.0)
            .collect())
    }
}

/// serde functions for handling HashMap with bytes value
pub mod hex_hashmap_value {
    use std::collections::HashMap;

    use serde::{ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

    use super::{deserialize_map, HexBuffer, HexValue};
    use crate::Bytes;

    pub fn serialize<S, K>(x: &HashMap<K, Bytes>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        K: Serialize,
    {
        let mut value_hex = HexBuffer::default();
        let mut map = s.serialize_map(Some(x.len()))?;
        for (k, v) in x.iter() {
            map.serialize_entry(k, value_hex.encode(v)?)?;
        }
        map.end()
    }

    pub fn deserialize<'de, K, D>(d: D) -> Result<HashMap<K, Bytes>, D::Error>
    where
        D: Deserializer<'de>,
        K: Deserialize<'de> + Eq + std::hash::Hash, // HashMap key trait bounds
    {
        deserialize_map::<D, K, HexValue, K, Bytes>(d)
    }
}

/// serde functions for handling HashMap with a bytes key and value
pub mod hex_hashmap_key_value {
    use std::collections::HashMap;

    use serde::{ser::SerializeMap, Deserializer, Serializer};

    use super::{deserialize_map, HexBuffer, HexValue};
    use crate::Bytes;

    pub fn serialize<S>(x: &HashMap<Bytes, Bytes>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (mut key_hex, mut value_hex) = (HexBuffer::default(), HexBuffer::default());
        let mut map = s.serialize_map(Some(x.len()))?;
        for (k, v) in x.iter() {
            map.serialize_entry(key_hex.encode(k)?, value_hex.encode(v)?)?;
        }
        map.end()
    }

    pub fn deserialize<'de, D>(d: D) -> Result<HashMap<Bytes, Bytes>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_map::<D, HexValue, HexValue, Bytes, Bytes>(d)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::Bytes;

    #[derive(Debug, Serialize, Deserialize)]
    struct TestStruct {
        #[serde(with = "hex_bytes")]
        bytes: Vec<u8>,

        #[serde(with = "hex_bytes_option")]
        bytes_option: Option<Vec<u8>>,

        #[serde(with = "hex_bytes_vec")]
        bytes_vec: Vec<Vec<u8>>,
    }

    #[test]
    fn hex_bytes_serialize_deserialize() {
        let test_struct = TestStruct {
            bytes: vec![0u8; 10],
            bytes_option: Some(vec![0u8; 10]),
            bytes_vec: vec![vec![0, 1, 2, 3], vec![0xFF, 0xAB]],
        };

        // Serialize to JSON
        let serialized = serde_json::to_string(&test_struct).unwrap();
        assert_eq!(
            serialized,
            "{\"bytes\":\"0x00000000000000000000\",\"bytes_option\":\"0x00000000000000000000\",\"bytes_vec\":[\"0x00010203\",\"0xffab\"]}"
        );

        // Deserialize from JSON
        let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.bytes, vec![0u8; 10]);
        assert_eq!(deserialized.bytes_option, Some(vec![0u8; 10]));
        assert_eq!(deserialized.bytes_vec, vec![vec![0, 1, 2, 3], vec![0xFF, 0xAB]]);
    }

    #[test]
    fn hex_bytes_option_none() {
        let test_struct =
            TestStruct { bytes: vec![0u8; 10], bytes_option: None, bytes_vec: vec![] };

        // Serialize to JSON
        let serialized = serde_json::to_string(&test_struct).unwrap();
        assert_eq!(
            serialized,
            "{\"bytes\":\"0x00000000000000000000\",\"bytes_option\":null,\"bytes_vec\":[]}"
        );

        // Deserialize from JSON
        let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.bytes, vec![0u8; 10]);
        assert_eq!(deserialized.bytes_option, None);
    }

    /// Serializers that format every value with `format!`: the expected output for the ones above.
    mod reference {
        use std::collections::HashMap;

        use serde::{ser::SerializeMap, Serialize, Serializer};

        use crate::Bytes;

        pub fn hex(x: &[u8]) -> String {
            format!("0x{}", hex::encode(x))
        }

        pub fn hex_bytes<S: Serializer>(x: &[u8], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&hex(x))
        }

        pub fn hex_bytes_option<S: Serializer>(
            x: &Option<Vec<u8>>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match x {
                Some(x) => s.serialize_str(&hex(x)),
                None => s.serialize_none(),
            }
        }

        pub fn hex_bytes_vec<S: Serializer>(list: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
            list.iter()
                .map(|x| hex(x))
                .collect::<Vec<_>>()
                .serialize(s)
        }

        pub fn hex_hashmap_key<S: Serializer>(
            x: &HashMap<Bytes, Bytes>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            let mut map = s.serialize_map(Some(x.len()))?;
            for (k, v) in x.iter() {
                map.serialize_entry(&format!("{k:#x}"), v)?;
            }
            map.end()
        }

        pub fn hex_hashmap_value<S: Serializer>(
            x: &HashMap<Bytes, Bytes>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            let mut map = s.serialize_map(Some(x.len()))?;
            for (k, v) in x.iter() {
                map.serialize_entry(k, &format!("{v:#x}"))?;
            }
            map.end()
        }

        pub fn hex_hashmap_key_value<S: Serializer>(
            x: &HashMap<Bytes, Bytes>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            let mut map = s.serialize_map(Some(x.len()))?;
            for (k, v) in x.iter() {
                map.serialize_entry(&format!("{k:#x}"), &format!("{v:#x}"))?;
            }
            map.end()
        }

        /// Decodes by copying the digits and prepending a zero to an odd count.
        pub fn decode(val: &str) -> Result<Vec<u8>, hex::FromHexError> {
            let mut digits: String = val
                .strip_prefix("0x")
                .unwrap_or(val)
                .into();
            if !digits.len().is_multiple_of(2) {
                digits.insert(0, '0');
            }
            hex::decode(&digits)
        }
    }

    #[derive(Serialize)]
    struct CurrentBytes<'a> {
        #[serde(with = "hex_bytes")]
        bytes: &'a Vec<u8>,
        #[serde(with = "hex_bytes_option")]
        bytes_option: &'a Option<Vec<u8>>,
        #[serde(with = "hex_bytes_vec")]
        bytes_vec: &'a Vec<Vec<u8>>,
    }

    #[derive(Serialize)]
    struct ReferenceBytes<'a> {
        #[serde(serialize_with = "reference::hex_bytes")]
        bytes: &'a Vec<u8>,
        #[serde(serialize_with = "reference::hex_bytes_option")]
        bytes_option: &'a Option<Vec<u8>>,
        #[serde(serialize_with = "reference::hex_bytes_vec")]
        bytes_vec: &'a Vec<Vec<u8>>,
    }

    #[derive(Serialize)]
    struct CurrentMaps<'a> {
        #[serde(with = "hex_hashmap_key")]
        key: &'a HashMap<Bytes, Bytes>,
        #[serde(with = "hex_hashmap_value")]
        value: &'a HashMap<Bytes, Bytes>,
        #[serde(with = "hex_hashmap_key_value")]
        key_value: &'a HashMap<Bytes, Bytes>,
    }

    #[derive(Serialize)]
    struct ReferenceMaps<'a> {
        #[serde(serialize_with = "reference::hex_hashmap_key")]
        key: &'a HashMap<Bytes, Bytes>,
        #[serde(serialize_with = "reference::hex_hashmap_value")]
        value: &'a HashMap<Bytes, Bytes>,
        #[serde(serialize_with = "reference::hex_hashmap_key_value")]
        key_value: &'a HashMap<Bytes, Bytes>,
    }

    /// Empty, single bytes, leading zeros, a full word, and values longer than any slot: the
    /// largest contract code, odd lengths and 128 KiB.
    fn hex_cases() -> Vec<Vec<u8>> {
        let long = |len: usize| {
            (0..len)
                .map(|i| (i * 31 % 256) as u8)
                .collect()
        };
        vec![
            vec![],
            vec![0],
            vec![0x0a],
            vec![0xff],
            vec![0, 0, 1],
            vec![0xff; 32],
            (0..32).collect(),
            long(33),
            long(24_576),
            long(24_577),
            long((1 << 17) + 3),
        ]
    }

    #[test]
    fn hex_bytes_serializers_write_the_same_json_as_formatting_each_value() {
        let cases = hex_cases();
        for value in &cases {
            let option = Some(value.clone());
            let current = CurrentBytes { bytes: value, bytes_option: &option, bytes_vec: &cases };
            let reference =
                ReferenceBytes { bytes: value, bytes_option: &option, bytes_vec: &cases };

            assert!(
                serde_json::to_vec(&current).unwrap() == serde_json::to_vec(&reference).unwrap(),
                "output differs for a {}-byte value",
                value.len()
            );
        }
    }

    #[test]
    fn hex_hashmap_serializers_write_the_same_json_as_formatting_each_entry() {
        let cases = hex_cases();
        // Each key is a case followed by its index and a value prefix, so that keys are distinct
        // and short and long keys sit next to short and long values.
        let mut map = HashMap::new();
        for (i, key) in cases.iter().enumerate() {
            for value in &cases {
                let mut key = key.clone();
                key.push(i as u8);
                key.extend_from_slice(&value[..value.len().min(40)]);
                map.insert(Bytes::from(key), Bytes::from(value.clone()));
            }
        }

        let current = CurrentMaps { key: &map, value: &map, key_value: &map };
        let reference = ReferenceMaps { key: &map, value: &map, key_value: &map };

        assert!(serde_json::to_vec(&current).unwrap() == serde_json::to_vec(&reference).unwrap());
    }

    #[test]
    fn hex_serializers_round_trip_long_values() {
        let test_struct = TestStruct {
            bytes: hex_cases().pop().unwrap(),
            bytes_option: Some(vec![]),
            bytes_vec: hex_cases(),
        };

        let serialized = serde_json::to_string(&test_struct).unwrap();
        let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.bytes, test_struct.bytes);
        assert_eq!(deserialized.bytes_option, Some(vec![]));
        assert_eq!(deserialized.bytes_vec, test_struct.bytes_vec);
    }

    /// Every case as lowercase and uppercase digits, with and without the prefix, and with an odd
    /// number of digits where the first one is a zero.
    fn hex_strings() -> Vec<String> {
        let mut strings = vec![];
        for value in hex_cases() {
            let digits = hex::encode(&value);
            for digits in [digits.clone(), digits.to_uppercase()] {
                strings.push(format!("0x{digits}"));
                strings.push(digits.clone());
                if let Some(odd) = digits.strip_prefix('0') {
                    strings.push(format!("0x{odd}"));
                    strings.push(odd.to_string());
                }
            }
        }
        strings
    }

    #[test]
    fn decode_hex_with_prefix_reads_the_same_bytes_as_the_reference_decoder() {
        for string in hex_strings() {
            assert_eq!(
                decode_hex_with_prefix(&string).unwrap(),
                reference::decode(&string).unwrap(),
                "differs for {}",
                &string[..string.len().min(20)]
            );
        }
    }

    #[test]
    fn decode_hex_with_prefix_rejects_what_the_reference_decoder_rejects() {
        for string in ["0xzz", "0x1g", "0xg", "g", "0x0x", "é", "0xé", "0x0é", "é0", "0x é", "0x-1"]
        {
            assert!(reference::decode(string).is_err(), "reference accepts {string}");
            assert!(decode_hex_with_prefix(string).is_err(), "accepts {string}");
        }
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct DecodedMaps {
        #[serde(with = "hex_hashmap_key")]
        key: HashMap<Bytes, u64>,
        #[serde(with = "hex_hashmap_value")]
        value: HashMap<String, Bytes>,
        #[serde(with = "hex_hashmap_key_value")]
        key_value: HashMap<Bytes, Bytes>,
    }

    #[test]
    fn hex_deserializers_read_borrowed_escaped_and_owned_strings() {
        // "\u0030x0a" is "0x0a" with an escaped first character: the deserializer can't lend it
        // from the input.
        let json = r#"{
            "key": {"0x0a": 1, "\u0030x0b": 2},
            "value": {"a": "0x0c", "b": "\u0030x0d"},
            "key_value": {"0x01": "0x0002", "3": "0xff", "\u0030x04": "0x"}
        }"#;
        let expected = DecodedMaps {
            key: HashMap::from([(Bytes::from(vec![0x0a]), 1), (Bytes::from(vec![0x0b]), 2)]),
            value: HashMap::from([
                ("a".to_string(), Bytes::from(vec![0x0c])),
                ("b".to_string(), Bytes::from(vec![0x0d])),
            ]),
            key_value: HashMap::from([
                (Bytes::from(vec![0x01]), Bytes::from(vec![0x00, 0x02])),
                (Bytes::from(vec![0x03]), Bytes::from(vec![0xff])),
                (Bytes::from(vec![0x04]), Bytes::from(vec![])),
            ]),
        };

        let from_str: DecodedMaps = serde_json::from_str(json).unwrap();
        let from_reader: DecodedMaps = serde_json::from_reader(json.as_bytes()).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let from_value: DecodedMaps = serde_json::from_value(value).unwrap();

        assert_eq!(from_str, expected);
        assert_eq!(from_reader, expected);
        assert_eq!(from_value, expected);
    }

    #[test]
    fn hex_deserializers_reject_invalid_hex_and_non_strings() {
        let invalid = [
            r#"{"key": {"0xzz": 1}, "value": {}, "key_value": {}}"#,
            r#"{"key": {}, "value": {"a": "0x1g"}, "key_value": {}}"#,
            r#"{"key": {}, "value": {}, "key_value": {"0x01": "é"}}"#,
            r#"{"key": {}, "value": {"a": 1}, "key_value": {}}"#,
        ];
        for json in invalid {
            assert!(serde_json::from_str::<DecodedMaps>(json).is_err(), "accepts {json}");
        }
        assert!(serde_json::from_str::<TestStruct>(
            r#"{"bytes": 1, "bytes_option": null, "bytes_vec": []}"#
        )
        .is_err());
    }
}
