/// serde functions for handling bytes as hex strings, such as `bytes::Bytes`
pub mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize a byte vec as a hex string with 0x prefix
    pub fn serialize<S, T>(x: T, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: AsRef<[u8]>,
    {
        s.serialize_str(&format!("0x{encoded}", encoded = hex::encode(x.as_ref())))
    }

    /// Deserialize a hex string into a byte vec
    /// Accepts a hex string with optional 0x prefix
    pub fn deserialize<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: From<Vec<u8>>,
    {
        let value = String::deserialize(d)?;
        if let Some(value) = value.strip_prefix("0x") {
            hex::decode(value)
        } else {
            hex::decode(&value)
        }
        .map(Into::into)
        .map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// serde functions for handling Option of bytes
pub mod hex_bytes_option {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize a byte vec as a Some hex string with 0x prefix
    pub fn serialize<S, T>(x: &Option<T>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: AsRef<[u8]>,
    {
        if let Some(x) = x {
            s.serialize_str(&format!("0x{encoded}", encoded = hex::encode(x.as_ref())))
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
            Some(val) => {
                let val = if let Some(stripped) = val.strip_prefix("0x") { stripped } else { &val };
                hex::decode(val)
                    .map(Into::into)
                    .map(Some)
                    .map_err(|e| serde::de::Error::custom(e.to_string()))
            }
            None => Ok(None),
        }
    }
}

/// Serde helpers for `HashMap<String, Box<dyn ProtocolSim>>`.
///
/// Some `ProtocolSim` implementations (VM-backed states) return errors from
/// their `Serialize` impl. This module provides a custom serializer that
/// gracefully skips those entries instead of failing the entire map.
pub mod protocol_states {
    use std::collections::HashMap;

    use serde::{ser::SerializeMap, Deserialize, Deserializer, Serializer};
    use tracing::{debug, warn};
    use tycho_common::simulation::protocol_sim::ProtocolSim;

    /// Serializes a map of `ProtocolSim` trait objects, skipping entries
    /// whose `Serialize` impl returns an error (e.g., VM-backed states).
    pub fn serialize<S>(
        states: &HashMap<String, Box<dyn ProtocolSim>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        let mut skipped = 0u32;
        for (key, value) in states {
            match serde_json::to_value(value.as_ref()) {
                Ok(json_val) => map.serialize_entry(key, &json_val)?,
                Err(err) => {
                    debug!(key, %err, "skipping non-serializable ProtocolSim entry");
                    skipped += 1;
                }
            }
        }
        if skipped > 0 {
            warn!(skipped, total = states.len(), "skipped non-serializable ProtocolSim entries");
        }
        map.end()
    }

    /// Deserializes back into the map. Non-serializable states are simply
    /// absent from the data, so default deserialization works.
    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<HashMap<String, Box<dyn ProtocolSim>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        HashMap::<String, Box<dyn ProtocolSim>>::deserialize(deserializer)
    }
}

/// Macro to implement error-returning Serialize/Deserialize for protocols
/// that cannot be serialized (e.g., due to VM state or external SDK dependencies).
///
/// # Examples
/// ```ignore
/// impl_non_serializable_protocol!(MyProtocolState, "error message");
/// ```
#[macro_export]
macro_rules! impl_non_serializable_protocol {
    ($type:ty, $msg:expr) => {
        impl serde::Serialize for $type {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom($msg))
            }
        }

        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Err(serde::de::Error::custom($msg))
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::{Deserialize, Serialize};
    use serde_json;
    use tycho_common::simulation::protocol_sim::ProtocolSim;

    use super::*;
    use crate::protocol::models::Update;

    #[derive(Debug, Serialize, Deserialize)]
    struct TestStruct {
        #[serde(with = "hex_bytes")]
        bytes: Vec<u8>,

        #[serde(with = "hex_bytes_option")]
        bytes_option: Option<Vec<u8>>,
    }

    #[test]
    fn hex_bytes_serialize_deserialize() {
        let test_struct = TestStruct { bytes: vec![0u8; 10], bytes_option: Some(vec![0u8; 10]) };

        // Serialize to JSON
        let serialized = serde_json::to_string(&test_struct).unwrap();
        assert_eq!(
            serialized,
            "{\"bytes\":\"0x00000000000000000000\",\"bytes_option\":\"0x00000000000000000000\"}"
        );

        // Deserialize from JSON
        let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.bytes, vec![0u8; 10]);
        assert_eq!(deserialized.bytes_option, Some(vec![0u8; 10]));
    }

    #[test]
    fn hex_bytes_option_none() {
        let test_struct = TestStruct { bytes: vec![0u8; 10], bytes_option: None };

        // Serialize to JSON
        let serialized = serde_json::to_string(&test_struct).unwrap();
        assert_eq!(serialized, "{\"bytes\":\"0x00000000000000000000\",\"bytes_option\":null}");

        // Deserialize from JSON
        let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.bytes, vec![0u8; 10]);
        assert_eq!(deserialized.bytes_option, None);
    }

    #[cfg(feature = "evm")]
    #[test]
    fn update_roundtrip_with_serializable_state() {
        use alloy::primitives::U256;

        use crate::evm::protocol::uniswap_v2::state::UniswapV2State;

        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        states.insert(
            "pool_a".to_string(),
            Box::new(UniswapV2State::new(U256::from(1000), U256::from(2000))),
        );

        let update = Update::new(12345, states, HashMap::new());
        let json = serde_json::to_string(&update).unwrap();

        assert!(json.contains("pool_a"));

        let roundtripped: Update = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.block_number_or_timestamp, 12345);
        assert_eq!(roundtripped.states.len(), 1);
        assert!(roundtripped
            .states
            .contains_key("pool_a"));
    }

    #[cfg(feature = "evm")]
    #[test]
    fn protocol_states_skips_non_serializable_entries() {
        use alloy::primitives::U256;

        use crate::evm::{decoder::MockProtocolSim, protocol::uniswap_v2::state::UniswapV2State};

        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        states.insert(
            "serializable".to_string(),
            Box::new(UniswapV2State::new(U256::from(1000), U256::from(2000))),
        );
        // MockProtocolSim's Serialize always errors, so it must be skipped by the
        // protocol_states serializer.
        states.insert("non_serializable".to_string(), Box::new(MockProtocolSim::new()));

        let update = Update::new(42, states, HashMap::new());
        let json = serde_json::to_string(&update).expect("serialization should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        let states_map = parsed["states"]
            .as_object()
            .expect("states is a map");
        assert_eq!(states_map.len(), 1, "only the serializable entry should survive");
        assert!(states_map.contains_key("serializable"));
        assert!(!states_map.contains_key("non_serializable"));
    }

    #[cfg(feature = "evm")]
    #[test]
    fn protocol_states_serialize_produces_valid_json() {
        use alloy::primitives::U256;

        use crate::evm::protocol::uniswap_v2::state::UniswapV2State;

        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        states.insert(
            "pool_x".to_string(),
            Box::new(UniswapV2State::new(U256::from(100), U256::from(200))),
        );
        states.insert(
            "pool_y".to_string(),
            Box::new(UniswapV2State::new(U256::from(300), U256::from(400))),
        );

        #[derive(Serialize)]
        struct Wrapper {
            #[serde(with = "protocol_states")]
            states: HashMap<String, Box<dyn ProtocolSim>>,
        }

        let wrapper = Wrapper { states };
        let json = serde_json::to_value(&wrapper).unwrap();
        let map = json["states"].as_object().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("pool_x"));
        assert!(map.contains_key("pool_y"));
    }

    #[test]
    fn update_roundtrip_empty() {
        let update = Update::new(99999, HashMap::new(), HashMap::new());
        let json = serde_json::to_string(&update).unwrap();
        let roundtripped: Update = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.block_number_or_timestamp, 99999);
        assert!(roundtripped.states.is_empty());
        assert!(roundtripped.new_pairs.is_empty());
    }
}
