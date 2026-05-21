use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use tracing::info;
use tycho_client::{
    rpc::{AllTokensParams, HttpRPCClientOptions, RPCClient, RPC_CLIENT_CONCURRENCY},
    HttpRPCClient, RPCError,
};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::errors::SimulationError,
    Bytes,
};

/// Converts a hexadecimal string into a `Vec<u8>`.
///
/// This function accepts a hexadecimal string with or without the `0x` prefix. If the prefix
/// is present, it is removed before decoding. The remaining string is expected to be a valid
/// hexadecimal representation, otherwise an error is returned.
///
/// # Arguments
///
/// * `hexstring` - A string slice containing the hexadecimal string. It may optionally start with
///   `0x`.
///
/// # Returns
///
/// * `Ok(Vec<u8>)` - A vector of bytes decoded from the hexadecimal string.
/// * `Err(SimulationError)` - An error if the input string is not a valid hexadecimal
///   representation.
///
/// # Errors
///
/// This function returns a `SimulationError::FatalError` if:
/// - The string contains invalid hexadecimal characters.
/// - The string is empty or malformed.
pub fn hexstring_to_vec(hexstring: &str) -> Result<Vec<u8>, SimulationError> {
    let hexstring_no_prefix =
        if let Some(stripped) = hexstring.strip_prefix("0x") { stripped } else { hexstring };
    let bytes = hex::decode(hexstring_no_prefix).map_err(|err| {
        SimulationError::FatalError(format!("Invalid hex string `{hexstring}`: {err}"))
    })?;
    Ok(bytes)
}

/// Loads all tokens from Tycho and returns them as a Hashmap of address->Token.
///
/// # Arguments
///
/// * `tycho_url` - The URL of the Tycho RPC (do not include the url prefix e.g. 'https://').
/// * `no_tls` - Whether to use HTTP instead of HTTPS.
/// * `auth_key` - The API key to use for authentication.
/// * `chain` - The chain to load tokens from.
/// * `min_quality` - The minimum quality of tokens to load. Defaults to 100 if not provided.
/// * `max_days_since_last_trade` - The max number of days since the token was last traded. Defaults
///   are chain specific and applied if not provided.
///
/// # Returns
///
/// * `Ok(HashMap<Bytes, Token>)` - A mapping from token address to token metadata loaded from Tycho
/// * `Err(SimulationError)` - An error indicating why the token list could not be loaded.
pub async fn load_all_tokens(
    tycho_url: &str,
    no_tls: bool,
    auth_key: Option<&str>,
    compression: bool,
    chain: Chain,
    min_quality: Option<i32>,
    max_days_since_last_trade: Option<u64>,
) -> Result<HashMap<Bytes, Token>, SimulationError> {
    info!("Loading tokens from Tycho...");
    let rpc_url =
        if no_tls { format!("http://{tycho_url}") } else { format!("https://{tycho_url}") };

    let rpc_options = HttpRPCClientOptions::new()
        .with_auth_key(auth_key.map(|s| s.to_string()))
        .with_compression(compression);

    let rpc_client = HttpRPCClient::new(rpc_url.as_str(), rpc_options)
        .map_err(|err| map_rpc_error(err, "Failed to create Tycho RPC client"))?;

    // Chain specific defaults for special case chains. Otherwise defaults to 42 days.
    let default_min_days = HashMap::from([(Chain::Base, 1_u64), (Chain::Unichain, 14_u64)]);

    #[allow(clippy::mutable_key_type)]
    let min_q = min_quality.or(Some(100));
    let traded = max_days_since_last_trade.or(default_min_days
        .get(&chain)
        .or(Some(&42))
        .copied());
    let mut token_params = AllTokensParams::new(chain, RPC_CLIENT_CONCURRENCY);
    if let Some(q) = min_q {
        token_params = token_params.with_min_quality(q);
    }
    if let Some(d) = traded {
        token_params = token_params.with_traded_n_days_ago(d);
    }
    let tokens = rpc_client
        .get_all_tokens(token_params)
        .await
        .map_err(|err| map_rpc_error(err, "Unable to load tokens"))?;

    tokens
        .into_iter()
        .map(|token| Ok((token.address.clone(), token)))
        .collect()
}

/// Get the default Tycho URL for the given chain.
pub fn get_default_url(chain: &Chain) -> Option<String> {
    match chain {
        Chain::Ethereum => Some("tycho-beta.propellerheads.xyz".to_string()),
        Chain::Base => Some("tycho-base-beta.propellerheads.xyz".to_string()),
        Chain::Unichain => Some("tycho-unichain-beta.propellerheads.xyz".to_string()),
        Chain::Bsc => Some("tycho-bsc-beta.propellerheads.xyz".to_string()),
        Chain::Arbitrum => Some("tycho-arbitrum-beta.propellerheads.xyz".to_string()),
        _ => None,
    }
}

/// Loads blocklisted component IDs from a TOML file.
///
/// If `path` is `Some`, reads from that file and returns an error if
/// the file is missing or invalid. If `None`, returns the default
/// blocklist embedded in the library.
pub fn load_blocklist(path: Option<&Path>) -> Result<HashSet<String>, SimulationError> {
    let Some(path) = path else {
        return Ok(default_blocklist());
    };
    let contents = fs::read_to_string(path).map_err(|e| {
        SimulationError::FatalError(format!("Failed to read blocklist {:?}: {e}", path))
    })?;
    parse_blocklist(&contents).map_err(|e| {
        SimulationError::FatalError(format!("Failed to parse blocklist {:?}: {e}", path))
    })
}

pub fn default_blocklist() -> HashSet<String> {
    parse_blocklist(include_str!("../blocklist.toml"))
        .expect("embedded blocklist.toml is valid TOML")
}

fn parse_blocklist(contents: &str) -> Result<HashSet<String>, toml::de::Error> {
    #[derive(Default, serde::Deserialize)]
    struct Blocklist {
        #[serde(default)]
        components: HashSet<String>,
    }

    #[derive(serde::Deserialize)]
    struct BlocklistConfig {
        #[serde(default)]
        blocklist: Blocklist,
    }

    let config: BlocklistConfig = toml::from_str(contents)?;
    Ok(config.blocklist.components)
}

fn map_rpc_error(err: RPCError, context: &str) -> SimulationError {
    let message = format!("{context}: {err}", err = err,);
    match err {
        RPCError::UrlParsing(_, _) | RPCError::FormatRequest(_) => {
            SimulationError::InvalidInput(message, None)
        }
        _ => SimulationError::FatalError(message),
    }
}
