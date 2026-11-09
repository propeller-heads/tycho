//! This module collects post processor components
//!
//! Usually changes or modifications required due to bugs in downstream substreams packages
//! that would require an expensive re-sync or similar. The post processors allow us to
//! avoid this by applying the necessary changes to the data after it has been extracted.

#![expect(
    deprecated,
    reason = "the deprecated UniswapV2 post processors stay registered for extractors configured with them"
)]

use std::collections::HashMap;

use attributes::{
    add_default_attributes_uniswapv2, add_default_attributes_uniswapv3, trim_curve_component_token,
};
use balances::{ignore_self_balances, transcode_ambient_balances, transcode_usv2_balances};
use once_cell::sync::Lazy;

use crate::extractor::models::BlockChanges;

mod attributes;
mod balances;

pub type PostProcessorFn = fn(BlockChanges) -> BlockChanges;

#[deprecated]
fn add_default_usv2_attributes_then_transcode_balances(input: BlockChanges) -> BlockChanges {
    transcode_usv2_balances(add_default_attributes_uniswapv2(input))
}

pub static POST_PROCESSOR_REGISTRY: Lazy<HashMap<String, PostProcessorFn>> = Lazy::new(|| {
    let mut registry = HashMap::new();
    registry.insert(
        "transcode_ambient_balances".to_string(),
        transcode_ambient_balances as PostProcessorFn,
    );
    registry.insert(
        "add_default_attributes_uniswapv3".to_string(),
        add_default_attributes_uniswapv3 as PostProcessorFn,
    );
    registry.insert("ignore_self_balances".to_string(), ignore_self_balances as PostProcessorFn);
    registry.insert(
        "trim_curve_component_token".to_string(),
        trim_curve_component_token as PostProcessorFn,
    );
    registry.insert(
        "add_default_usv2_attributes_then_transcode_balances".to_string(),
        add_default_usv2_attributes_then_transcode_balances as PostProcessorFn,
    );
    registry
});
