pub(super) mod cache;
pub(super) mod dci;
pub(super) mod hooks;

enum PausingReason {
    #[expect(dead_code, reason = "discriminant 1 is part of the stored pausing encoding")]
    Substreams,
    TracingError,
    MetadataError,
}

impl From<PausingReason> for u8 {
    fn from(reason: PausingReason) -> Self {
        match reason {
            PausingReason::Substreams => 1,
            PausingReason::TracingError => 2,
            PausingReason::MetadataError => 3,
        }
    }
}

impl From<PausingReason> for tycho_common::Bytes {
    fn from(reason: PausingReason) -> Self {
        match reason {
            PausingReason::Substreams => vec![1_u8].into(),
            PausingReason::TracingError => vec![2_u8].into(),
            PausingReason::MetadataError => vec![3_u8].into(),
        }
    }
}
