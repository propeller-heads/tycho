//! Logging makes it easier to investigate suspicious [Outcome](crate::Outcome)s,
//! by collecting and then printing important [Event]s.
//!
//! Logging creates some overhead so there's a [NopLog] which does nothing.
//! The compiler is good at removing dead code and should optimize [NopLog] away.
//!
//! Logging is useful for investigating suspicious outcomes once they are found.
//! If you're trying to find suspicious [Outcome](crate::Outcome)s in the first place,
//! performance matters and it's better to disable logging via the [NopLog].

use serde::{
    Serialize,
    ser::{SerializeMap, SerializeSeq},
};

use crate::{address::Address, model::executors::TransferData};

/// An important [Event] that makes it easier to reason about
/// the execution path the simulation took.
///
/// TODO this list is incomplete.
pub enum Event {
    UpdateDeltaAccounting {
        token: Address,
        delta_change: i64,
        nonzero_delta_count_after: u64,
        context_hint: &'static str,
    },
    CreditVault {
        owner: Address,
        token: Address,
        amount: i64,
        context_hint: &'static str,
    },
    DebitVault {
        owner: Address,
        token: Address,
        amount: i64,
        context_hint: &'static str,
    },
    TransferOut {
        receiver: Address,
        token: Address,
        amount: i64,
        context_hint: &'static str,
    },
    EthSendValue {
        sender: Address,
        receiver: Address,
        amount: i64,
        context_hint: &'static str,
    },
    Erc20SafeTransfer {
        token: Address,
        sender: Address,
        receiver: Address,
        amount: i64,
        context_hint: &'static str,
    },
    TransferData {
        transfer_data: TransferData,
        context_hint: &'static str,
    },
}

impl Event {
    fn context_hint(&self) -> &'static str {
        match self {
            Self::UpdateDeltaAccounting { context_hint, .. } => context_hint,
            Self::CreditVault { context_hint, .. } => context_hint,
            Self::DebitVault { context_hint, .. } => context_hint,
            Self::TransferOut { context_hint, .. } => context_hint,
            Self::EthSendValue { context_hint, .. } => context_hint,
            Self::Erc20SafeTransfer { context_hint, .. } => context_hint,
            Self::TransferData { context_hint, .. } => context_hint,
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateDeltaAccounting {
                token, delta_change, nonzero_delta_count_after, ..
            } => write!(
                f,
                "`_updateDeltaAccounting(token={token:?}, deltaChange={delta_change})` after which `_getNonZeroDeltaCount() == {nonzero_delta_count_after}`"
            ),
            Self::CreditVault { owner, token, amount, .. } => {
                write!(f, "_creditVault(user={owner:?}, token={token:?}, amount={amount})")
            }
            Self::DebitVault { owner, token, amount, .. } => {
                write!(f, "_debitVault(user={owner:?}, token={token:?}, amount={amount})")
            }
            Self::TransferOut { receiver, token, amount, .. } => {
                write!(f, "_transferOut(token={token:?}, to={receiver:?}, amount={amount})")
            }
            Self::EthSendValue { sender, receiver, amount, .. } => write!(
                f,
                "`{sender:?}` calls: `Address.sendValue(to={receiver:?}, amount={amount})`"
            ),
            Self::Erc20SafeTransfer { token, sender, receiver, amount, .. } => write!(
                f,
                "`{sender:?}` calls: `IERC20({token:?}).safeTransfer(to={receiver:?}, amount={amount})`"
            ),
            Self::TransferData { .. } => write!(f, "transferData"),
        }
    }
}

impl Serialize for Event {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_map(None)?;
        s.serialize_entry("event", &self.to_string())?;
        s.serialize_entry("context", &self.context_hint())?;
        if let Self::TransferData { transfer_data, .. } = self {
            s.serialize_entry("transfer_data", &transfer_data)?;
        }
        s.end()
    }
}

pub trait Log {
    fn append(&mut self, event: Event);

    fn is_empty(&self) -> bool;
}

/// A [Log] that's backed by a [Vec] to which [Event]s are appended.
/// Stores events but adds some overhead.
#[derive(Default)]
pub struct VecLog {
    events: Vec<Event>,
}

impl Log for VecLog {
    fn append(&mut self, event: Event) {
        self.events.push(event);
    }
    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Serialize for VecLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_seq(Some(self.events.len()))?;
        for event in &self.events {
            s.serialize_element(&event)?;
        }
        s.end()
    }
}

/// A [Log] that does nothing.
/// Does not store events but should result in zero overhead.
#[derive(Default)]
pub struct NopLog;

impl Log for NopLog {
    fn append(&mut self, _event: Event) {}
    fn is_empty(&self) -> bool {
        true
    }
}

impl Serialize for NopLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = serializer.serialize_seq(Some(0))?;
        s.end()
    }
}
