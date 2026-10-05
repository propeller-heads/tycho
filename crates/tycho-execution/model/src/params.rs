//! [Representation of a combination of parameters](Params) to simulate
//! and a [mechanism](RequestParam) by which the model can [request additional
//! parameters](Params::request) to be simulated.
use std::{borrow::Cow, rc::Rc};

use serde::Serialize;

use crate::{address::Address, model::executors::Executor};

/// Because [Params] contains [Rc] it isn't sendable to other threads.
/// We need to be able to send it to other threads.
/// This is just the inner part of [Params] which is sendable to other threads.
pub type ParamsInner = rustc_hash::FxHashMap<ParamKey, ParamValue>;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Params(pub Rc<ParamsInner>);

impl Params {
    /// If the parameter identified by `key` has a value in `self`,
    /// try to convert the value to the desired output type `V` and return it.
    /// Otherwise, return a [RequestParamError], which contains
    /// all necessary information to generate [Params] that will
    /// contain each of `variants` for `key` so the call to [Params::request]
    /// will not error the next time it is run.
    ///
    /// Simple example that uses [Params::request] to solve a system of linear equations:
    /// ```
    /// # use tycho_router_model::params::{Params, RequestParam, ParamKey, ParamValue, RequestParamError, ParamsInner};
    /// fn is_solution(params: &Params) -> Result<bool, RequestParamError> {
    ///     let x: i64 = params.request("x", [-3, -2, -1, 0, 1, 2, 3])?;
    ///     let y: i64 = params.request("y", [-3, -2, -1, 0, 1, 2, 3])?;
    ///     let z: i64 = params.request("z", [-3, -2, -1, 0, 1, 2, 3])?;
    ///
    ///     // the system of 3 linear equations
    ///     Ok(
    ///         3 * x + 2 * y - z == 1 &&
    ///         2 * x  - 2 * y + 4 * z == -2 &&
    ///         -x + y / 2 - z == 0
    ///     )
    /// }
    ///
    /// let mut queue = Vec::new();
    /// queue.push(Params::default());
    /// let mut solution: Option<(i64, i64, i64)> = None;
    /// while let Some(params) = queue.pop() {
    ///     match is_solution(&params) {
    ///         Ok(false) => {},
    ///         Ok(true) => {
    ///             solution = Some((
    ///                 params.get("x").unwrap(),
    ///                 params.get("y").unwrap(),
    ///                 params.get("z").unwrap()
    ///             ));
    ///             break;
    ///         }
    ///         Err(RequestParamError::RequestParam(request)) => for params in request {
    ///             queue.push(Params::from(params));
    ///         }
    ///         Err(err) => Err(err).unwrap(),
    ///     }
    /// }
    ///
    /// assert_eq!(solution, Some((1, -2, -2)));
    /// ```
    pub fn request<K: Into<ParamKey>, V: Into<ParamValue>, I: IntoIterator<Item = V>>(
        &self,
        key: K,
        variants: I,
    ) -> Result<V, RequestParamError>
    where
        ParamValue: TryInto<V>,
    {
        let key: ParamKey = key.into();
        if let Some(param) = self.0.get(&key) {
            let Ok(value) = param.clone().try_into() else {
                return Err(RequestParamError::ShouldBeConvertibleInto(key, param.clone()));
            };
            return Ok(value);
        }

        // turning this into a Vec right here is faster and simpler than
        // turning it into a trait object and iterating it later
        let variants: Vec<ParamValue> = variants
            .into_iter()
            .map(|x| x.into())
            .collect();
        if variants.is_empty() {
            return Err(RequestParamError::VariantsShouldNotBeEmptySinceWouldCauseInfiniteLoop(key));
        }

        Err(RequestParamError::RequestParam(RequestParam::new(self, key, variants)))
    }

    /// Return the value of the parameter with `key`.
    /// Return `None`, if the parameter is not present
    /// or cannot be converted into `V`.
    pub fn get<K: Into<ParamKey>, V: Into<ParamValue>>(&self, key: K) -> Option<V>
    where
        ParamValue: TryInto<V>,
    {
        let key: ParamKey = key.into();
        self.0
            .get(&key)
            .and_then(|x| x.clone().try_into().ok())
    }

    /// Return the list of [Executor]s for each executed swap in order.
    pub fn executors(&self) -> Vec<Executor> {
        let mut result = Vec::new();
        for swap_index in 0.. {
            match self.get(ParamKey::Executor { swap_index }) {
                Some(executor) => result.push(executor),
                None => {
                    break;
                }
            }
        }
        result
    }
}

impl From<ParamsInner> for Params {
    fn from(value: ParamsInner) -> Self {
        Params(Rc::new(value))
    }
}

/// Parameters can be [Address]es, [Executor]s, strings, integers, booleans, etc.
/// Parameters are stored as values in a map.
/// Maps can only store values of one specific type.
/// A single type is needed that can represent the required types parameters can take on.
/// This is that type.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(untagged)]
pub enum ParamValue {
    String(Cow<'static, str>),
    Address(Address),
    Bool(bool),
    I64(i64),
    Executor(Executor),
}

impl From<Address> for ParamValue {
    fn from(value: Address) -> Self {
        Self::Address(value)
    }
}

impl From<bool> for ParamValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for ParamValue {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<String> for ParamValue {
    fn from(value: String) -> Self {
        Self::String(value.into())
    }
}

impl From<&'static str> for ParamValue {
    fn from(value: &'static str) -> Self {
        Self::String(value.into())
    }
}

impl From<Executor> for ParamValue {
    fn from(value: Executor) -> Self {
        Self::Executor(value)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TryFromParamError;

impl TryInto<Address> for ParamValue {
    type Error = TryFromParamError;
    fn try_into(self) -> Result<Address, Self::Error> {
        match self {
            Self::Address(address) => Ok(address),
            _ => Err(TryFromParamError),
        }
    }
}

impl TryInto<bool> for ParamValue {
    type Error = TryFromParamError;
    fn try_into(self) -> Result<bool, Self::Error> {
        match self {
            Self::Bool(boolean) => Ok(boolean),
            _ => Err(TryFromParamError),
        }
    }
}

impl TryInto<i64> for ParamValue {
    type Error = TryFromParamError;
    fn try_into(self) -> Result<i64, Self::Error> {
        match self {
            Self::I64(number) => Ok(number),
            _ => Err(TryFromParamError),
        }
    }
}

impl TryInto<&'static str> for ParamValue {
    type Error = TryFromParamError;
    fn try_into(self) -> Result<&'static str, Self::Error> {
        match self {
            Self::String(Cow::Borrowed(x)) => Ok(x),
            _ => Err(TryFromParamError),
        }
    }
}

impl TryInto<Executor> for ParamValue {
    type Error = TryFromParamError;
    fn try_into(self) -> Result<Executor, Self::Error> {
        match self {
            Self::Executor(x) => Ok(x),
            _ => Err(TryFromParamError),
        }
    }
}

/// Type by which a parameter in [Params] is identified.
///
/// Initially [ParamKey] was simply a `Cow<&'static, str>`,
/// a type that can either be a borrowed `&'static str` or owned [String].
/// The key for the `n`th swap's [Executor], for example, had to be created
/// using `format!("executor[{n}]")`.
/// Since [String] makes a heap allocation this resulted in millions of expensive heap allocations
/// per second.
///
/// [ParamKey] never allocates on the heap,
/// stores only essential information,
/// and provides greater type safety than the previous approach.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Hash)]
pub enum ParamKey {
    String(&'static str),
    /// That has a static prefix and a dynamic index
    SwapIndexed {
        prefix: &'static str,
        swap_index: u8,
    },
    /// Executor used by swap with index `swap_index`.
    Executor {
        swap_index: u8,
    },
    /// Slice of the swap data of swap with index `swap_index`
    /// starting at byte `start` and ending at byte `end` (not inclusive).
    SwapData {
        swap_index: u8,
        start: u8,
        end: u8,
    },
    /// Slice of the protocol data of swap with index `swap_index`
    /// starting at byte `start` and ending at byte `end` (not inclusive).
    /// This corresponds to the [ParamKey::SwapData] after offset `25`.
    ProtocolData {
        swap_index: u8,
        start: u8,
        end: u8,
    },
    CallbackCalldata {
        swap_index: u8,
        start: u8,
        end: u8,
    },
    /// Token fee configured for `token`.
    TokenFeeBps {
        token: Address,
    },
}

impl std::fmt::Display for ParamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(string) => f.write_str(string),
            Self::SwapIndexed { prefix, swap_index } => write!(f, "{prefix}[{swap_index}]"),
            Self::Executor { swap_index } => write!(f, "executors[{swap_index}]"),
            Self::SwapData { swap_index, start, end } => {
                write!(f, "swap_data[{swap_index}][{start}:{end}]")
            }
            Self::ProtocolData { swap_index, start, end } => {
                write!(f, "protocol_data[{swap_index}][{start}:{end}]")
            }
            Self::CallbackCalldata { swap_index, start, end } => {
                write!(f, "callback_calldata[{swap_index}][{start}:{end}]")
            }
            Self::TokenFeeBps { token } => write!(f, "token_fee_bps[{token:?}]"),
        }
    }
}

impl Serialize for ParamKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl From<&'static str> for ParamKey {
    fn from(value: &'static str) -> Self {
        Self::String(value)
    }
}

#[derive(Debug)]
enum RequestParamInner {
    New(Rc<ParamsInner>, Vec<ParamValue>),
    Iterating(ParamsInner, std::vec::IntoIter<ParamValue>),
    Empty,
}

/// Request each of the `variants` of param `key` to be added to `params`.
/// Allows on-demand expansion of the parameter space.
///
/// Each [RequestParam] is itself an [Iterator] that generates the [ParamsInner] resulting from the
/// request: ```
/// # use tycho_router_model::Address;
/// # use tycho_router_model::model::executors::Executor;
/// # use tycho_router_model::params::{Params, RequestParam, ParamKey, ParamValue};
/// # use rustc_hash::FxHashMap;
///
/// let params = Params::default();
/// let mut request = RequestParam::new(&params, ParamKey::from("token"), vec![
///     ParamValue::from(Address::Zero),
///     ParamValue::from(Address::WETH),
///     ParamValue::from(Address::Router),
/// ]);
///
/// let params_0 = request.next().unwrap();
/// assert_eq!(params_0, FxHashMap::from_iter(vec![
///     (ParamKey::from("token"), ParamValue::from(Address::Zero))
/// ].into_iter()));
/// let params_1 = request.next().unwrap();
/// assert_eq!(params_1, FxHashMap::from_iter(vec![
///     (ParamKey::from("token"), ParamValue::from(Address::WETH))
/// ].into_iter()));
/// let params_2 = request.next().unwrap();
/// assert_eq!(params_2, FxHashMap::from_iter(vec![
///     (ParamKey::from("token"), ParamValue::from(Address::Router))
/// ].into_iter()));
/// assert_eq!(request.next(), None);
///
/// // expand params_2 even further
///
/// let mut request_2 = RequestParam::new(&Params::from(params_2), ParamKey::from("executor"), vec![
///     ParamValue::from(Executor::Curve),
///     ParamValue::from(Executor::Slipstreams),
/// ]);
///
/// let params_2_0 = request_2.next().unwrap();
/// assert_eq!(params_2_0, FxHashMap::from_iter(vec![
///     (ParamKey::from("token"), ParamValue::from(Address::Router)),
///     (ParamKey::from("executor"), ParamValue::from(Executor::Curve))
/// ].into_iter()));
/// let params_2_1 = request_2.next().unwrap();
/// assert_eq!(params_2_1, FxHashMap::from_iter(vec![
///     (ParamKey::from("token"), ParamValue::from(Address::Router)),
///     (ParamKey::from("executor"), ParamValue::from(Executor::Slipstreams))
/// ].into_iter()));
/// assert_eq!(request_2.next(), None);
/// ```
#[derive(Debug)]
pub struct RequestParam {
    key: ParamKey,
    inner: RequestParamInner,
}

impl RequestParam {
    /// Request each of the `variants` of param `key` to be added to `params`.
    ///
    /// You should use [Params::request] instead as it is less verbose to type
    /// and automatically converts the `key` and `variants` parameters
    /// from many types.
    /// This is just the constructor for [RequestParam].
    pub fn new(params: &Params, key: ParamKey, variants: Vec<ParamValue>) -> Self {
        assert!(!variants.is_empty());
        Self { key, inner: RequestParamInner::New(Rc::clone(&params.0), variants) }
    }
}

impl Iterator for RequestParam {
    type Item = ParamsInner;
    fn next(&mut self) -> Option<Self::Item> {
        match std::mem::replace(&mut self.inner, RequestParamInner::Empty) {
            RequestParamInner::New(params, variants) => {
                // once we reach this place, we're outside of `simulate`
                // and the other references to `params` are gone.
                // as a result, this will simply unwrap,
                // which saves an expensive clone operation.
                self.inner =
                    RequestParamInner::Iterating(Rc::unwrap_or_clone(params), variants.into_iter());
                self.next()
            }
            RequestParamInner::Iterating(mut params, mut variants) => {
                let variant = variants.next()?;
                // if there's only one variant left, there's no need to clone the map
                if variants.len() != 0 {
                    let params_clone = params.clone();
                    self.inner = RequestParamInner::Iterating(params_clone, variants);
                }
                params.insert(self.key, variant);
                Some(params)
            }
            RequestParamInner::Empty => None,
        }
    }
}

#[derive(Debug)]
pub enum RequestParamError {
    /// Used by [Params::request]
    /// to communicate that a parameter is missing and should be
    /// added to the parameter space.
    RequestParam(RequestParam),
    ShouldBeConvertibleInto(ParamKey, ParamValue),
    VariantsShouldNotBeEmptySinceWouldCauseInfiniteLoop(ParamKey),
}

impl std::fmt::Display for RequestParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestParam(x) => write!(f, "{x:?}"),
            Self::ShouldBeConvertibleInto(key, value) => write!(
                f,
                "the param named `{key:?}` whose value is `{value:?}` is not convertable into the desired type"
            ),
            Self::VariantsShouldNotBeEmptySinceWouldCauseInfiniteLoop(key) => write!(
                f,
                "the variants iterator should not be empty since that would cause an infinite loop for {key}"
            ),
        }
    }
}
