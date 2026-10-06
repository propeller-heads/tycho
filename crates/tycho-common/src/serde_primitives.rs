use hex::FromHexError;

fn decode_hex_with_prefix(val: &str) -> Result<Vec<u8>, FromHexError> {
    let mut stripped: String =
        if let Some(stripped) = val.strip_prefix("0x") { stripped } else { val }.into();

    // Check if the length of the string is odd
    if !stripped.len().is_multiple_of(2) {
        // If it's odd, prepend a zero
        stripped.insert(0, '0');
    }

    hex::decode(&stripped)
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

    use super::{decode_hex_with_prefix, HexBuffer};

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
        let value = String::deserialize(d)?;
        decode_hex_with_prefix(&value)
            .map(Into::into)
            .map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// serde functions for handling Option of bytes
pub mod hex_bytes_option {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{decode_hex_with_prefix, HexBuffer};

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
        let value: Option<String> = Option::deserialize(d)?;

        match value {
            Some(val) => decode_hex_with_prefix(&val)
                .map(Into::into)
                .map(Some)
                .map_err(|e| serde::de::Error::custom(e.to_string())),
            None => Ok(None),
        }
    }
}

/// serde functions for handling HashMap with a bytes key
pub mod hex_hashmap_key {
    use std::collections::HashMap;

    use serde::{de, ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

    use super::{decode_hex_with_prefix, HexBuffer};
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
        let interim = HashMap::<String, V>::deserialize(d)?;

        interim
            .into_iter()
            .map(|(k, v)| {
                let k = decode_hex_with_prefix(&k).map_err(|e| de::Error::custom(e.to_string()))?;
                Ok((Bytes::from(k), v))
            })
            .collect::<Result<HashMap<_, _>, _>>()
    }
}

/// serde functions for handling Vec of Bytes as hex strings
pub mod hex_bytes_vec {
    use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serializer};

    use super::{decode_hex_with_prefix, HexBuffer};

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
        let hex_strings = Vec::<String>::deserialize(d)?;
        hex_strings
            .into_iter()
            .map(|s| {
                decode_hex_with_prefix(&s).map_err(|e| serde::de::Error::custom(e.to_string()))
            })
            .collect()
    }
}

/// serde functions for handling HashMap with bytes value
pub mod hex_hashmap_value {
    use std::collections::HashMap;

    use serde::{de, ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

    use super::{decode_hex_with_prefix, HexBuffer};
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
        let interim = HashMap::<K, String>::deserialize(d)?;

        interim
            .into_iter()
            .map(|(k, v)| {
                let v = decode_hex_with_prefix(&v).map_err(|e| de::Error::custom(e.to_string()))?;
                Ok((k, Bytes::from(v)))
            })
            .collect::<Result<HashMap<_, _>, _>>()
    }
}

/// serde functions for handling HashMap with a bytes key and value
pub mod hex_hashmap_key_value {
    use std::collections::HashMap;

    use serde::{de, ser::SerializeMap, Deserialize, Deserializer, Serializer};

    use super::{decode_hex_with_prefix, HexBuffer};
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
        let interim = HashMap::<String, String>::deserialize(d)?;
        interim
            .into_iter()
            .map(|(k, v)| {
                let k = decode_hex_with_prefix(&k).map_err(|e| de::Error::custom(e.to_string()))?;
                let v = decode_hex_with_prefix(&v).map_err(|e| de::Error::custom(e.to_string()))?;
                Ok((k.into(), v.into()))
            })
            .collect::<Result<HashMap<_, _>, _>>()
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
}
