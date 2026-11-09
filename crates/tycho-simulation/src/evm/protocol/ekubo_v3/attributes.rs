use std::{borrow::Cow, collections::HashMap};

use ekubo_sdk::quoting::types::{Tick, TimeRateDelta};
use itertools::Itertools;
use tycho_common::Bytes;

pub fn ticks_from_attributes<'a, T: IntoIterator<Item = (impl AsRef<str>, Cow<'a, Bytes>)>>(
    attributes: T,
) -> Result<Vec<Tick>, String> {
    attributes
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.as_ref();
            // Support both new "tick/" and legacy "ticks/" prefix
            (key.starts_with("tick/") || key.starts_with("ticks/")).then(|| {
                let index = key
                    .split('/')
                    .nth(1)
                    .ok_or_else(|| "expected key name to contain tick index".to_string())?
                    .parse::<i32>()
                    .map_err(|err| format!("tick index can't be parsed as i32: {err}"))?;
                let liquidity_delta =
                    i128::try_from(value.into_owned()).map_err(|err| err.to_string())?;
                Ok(Tick { index, liquidity_delta })
            })
        })
        .try_collect()
}

pub fn rate_deltas_from_attributes<
    'a,
    T: IntoIterator<Item = (impl AsRef<str>, Cow<'a, Bytes>)>,
>(
    attributes: T,
    last_execution_time: u64,
) -> Result<impl Iterator<Item = TimeRateDelta>, String> {
    Ok(attributes
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.as_ref();

            // Support both new "rate_delta/" and legacy "orders/" prefix
            if !key.starts_with("rate_delta/") && !key.starts_with("orders/") {
                return None;
            }

            let Some((token_str, time_str)) = key.split('/').skip(1).collect_tuple() else {
                return Some(Err(format!(
                    "failed to parse rate delta attribute segments of \"{key}\""
                )));
            };

            let time: u64 = match time_str.parse() {
                Ok(time) => time,
                Err(err) => {
                    return Some(Err(format!("rate_delta time can't be parsed as u64: {err}")))
                }
            };

            if time <= last_execution_time {
                return None;
            }

            let is_token1 = match token_str {
                "token0" => false,
                "token1" => true,
                token => {
                    return Some(Err(format!(
                        r#"expected "token0" or "token1" but received "{token}""#
                    )))
                }
            };

            let delta = match i128::try_from(value.into_owned()) {
                Ok(delta) => delta,
                Err(err) => return Some(Err(err.to_string())),
            };

            Some(Ok((time, is_token1, delta)))
        })
        .try_collect::<_, Vec<_>, _>()?
        .into_iter()
        .fold(HashMap::new(), |mut map, (time, is_token1, value)| {
            let delta = map
                .entry(time)
                .or_insert(TimeRateDelta { time, rate_delta0: 0, rate_delta1: 0 });

            *(if is_token1 { &mut delta.rate_delta1 } else { &mut delta.rate_delta0 }) = value;

            map
        })
        .into_values())
}
