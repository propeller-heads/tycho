//! Decoding of the `swaps` / `swapData` byte payload into executor hops.
//!
//! Layouts (see `tycho-execution` strategy encoders):
//! - single: `executor(20) ++ protocol_data`
//! - sequential: PLE list of `executor(20) ++ protocol_data`
//! - split: PLE list of `tokenInIdx(1) ++ tokenOutIdx(1) ++ split(3, uint24) ++ executor(20) ++
//!   protocol_data`
//!
//! PLE ("prefixed length encoding") is `[len: u16 BE][data]` repeated.
use super::Method;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHop {
    pub executor: Vec<u8>,
    pub token_in_index: Option<u8>,
    pub token_out_index: Option<u8>,
    pub split: Option<u32>,
    pub protocol_data: Vec<u8>,
}

const ADDRESS_LEN: usize = 20;
const SPLIT_HEADER_LEN: usize = 1 + 1 + 3;

pub fn decode_hops(method: Method, swaps: &[u8]) -> Result<Vec<RawHop>, String> {
    match method {
        Method::Single => Ok(vec![decode_plain_hop(swaps)?]),
        Method::Sequential => ple_split(swaps)?
            .into_iter()
            .map(decode_plain_hop)
            .collect(),
        Method::Split => ple_split(swaps)?
            .into_iter()
            .map(decode_split_hop)
            .collect(),
    }
}

fn decode_plain_hop(data: &[u8]) -> Result<RawHop, String> {
    if data.len() < ADDRESS_LEN {
        return Err(format!(
            "hop payload of {} bytes is shorter than an executor address",
            data.len()
        ));
    }
    Ok(RawHop {
        executor: data[..ADDRESS_LEN].to_vec(),
        token_in_index: None,
        token_out_index: None,
        split: None,
        protocol_data: data[ADDRESS_LEN..].to_vec(),
    })
}

fn decode_split_hop(data: &[u8]) -> Result<RawHop, String> {
    if data.len() < SPLIT_HEADER_LEN + ADDRESS_LEN {
        return Err(format!(
            "split hop payload of {} bytes is shorter than its {}-byte header",
            data.len(),
            SPLIT_HEADER_LEN + ADDRESS_LEN
        ));
    }
    let split = u32::from_be_bytes([0, data[2], data[3], data[4]]);
    Ok(RawHop {
        executor: data[SPLIT_HEADER_LEN..SPLIT_HEADER_LEN + ADDRESS_LEN].to_vec(),
        token_in_index: Some(data[0]),
        token_out_index: Some(data[1]),
        split: Some(split),
        protocol_data: data[SPLIT_HEADER_LEN + ADDRESS_LEN..].to_vec(),
    })
}

fn ple_split(mut data: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut items = Vec::new();
    while !data.is_empty() {
        if data.len() < 2 {
            return Err("dangling byte in PLE-encoded swaps".to_string());
        }
        let len = u16::from_be_bytes([data[0], data[1]]) as usize;
        let Some(item) = data.get(2..2 + len) else {
            return Err(format!("PLE item declares {len} bytes but only {} remain", data.len() - 2));
        };
        items.push(item);
        data = &data[2 + len..];
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ple(items: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for item in items {
            out.extend((item.len() as u16).to_be_bytes());
            out.extend_from_slice(item);
        }
        out
    }

    #[test]
    fn single_hop_is_executor_plus_data() {
        let mut data = vec![0xaa; 20];
        data.extend([1, 2, 3]);
        let hops = decode_hops(Method::Single, &data).unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].executor, vec![0xaa; 20]);
        assert_eq!(hops[0].protocol_data, vec![1, 2, 3]);
        assert_eq!(hops[0].split, None);
    }

    #[test]
    fn single_hop_too_short_fails() {
        assert!(decode_hops(Method::Single, &[0u8; 19]).is_err());
    }

    #[test]
    fn sequential_hops_are_ple_encoded() {
        let hop_a = [vec![0x11; 20], vec![9]].concat();
        let hop_b = [vec![0x22; 20], vec![]].concat();
        let hops = decode_hops(Method::Sequential, &ple(&[&hop_a, &hop_b])).unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0].executor, vec![0x11; 20]);
        assert_eq!(hops[0].protocol_data, vec![9]);
        assert_eq!(hops[1].executor, vec![0x22; 20]);
        assert!(hops[1].protocol_data.is_empty());
    }

    #[test]
    fn split_hops_carry_indices_and_share() {
        let mut hop = vec![0, 2, 0x99, 0x99, 0x99];
        hop.extend([0x33; 20]);
        hop.extend([7, 7]);
        let hops = decode_hops(Method::Split, &ple(&[&hop])).unwrap();
        assert_eq!(hops[0].token_in_index, Some(0));
        assert_eq!(hops[0].token_out_index, Some(2));
        assert_eq!(hops[0].split, Some(0x999999));
        assert_eq!(hops[0].executor, vec![0x33; 20]);
        assert_eq!(hops[0].protocol_data, vec![7, 7]);
    }

    #[test]
    fn malformed_ple_fails() {
        assert!(decode_hops(Method::Sequential, &[0x00, 0x05, 1, 2]).is_err());
        assert!(decode_hops(Method::Sequential, &[0x01]).is_err());
        assert!(decode_hops(Method::Split, &ple(&[&[0u8; 10]])).is_err());
    }

    #[test]
    fn empty_sequential_payload_has_no_hops() {
        assert!(decode_hops(Method::Sequential, &[])
            .unwrap()
            .is_empty());
    }
}
