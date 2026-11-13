/// To run: cargo run --example run-analysis
use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy::primitives::U256;
use tycho_common::{
    models::{blockchain::BlockTag, token::TokenOwnerStore, Chain},
    traits::TokenAnalyzer,
    Bytes,
};
use tycho_ethereum::{
    rpc::EthereumRpcClient, services::token_analyzer::SimulationDetector, BytesCodec,
};

#[tokio::main]
async fn main() -> Result<(), ()> {
    let rpc_url = std::env::var("RPC_URL").expect("RPC URL must be set for testing");
    let rpc = EthereumRpcClient::new(&rpc_url).expect("RPC connection to Ethereum provider failed");

    let tf = TokenOwnerStore::new(HashMap::from([(
        Bytes::from_str("3A9FfF453d50D4Ac52A6890647b823379ba36B9E").unwrap(),
        (
            Bytes::from_str("260E069deAd76baAC587B5141bB606Ef8b9Bab6c").unwrap(),
            U256::from_str("13042252617814040589")
                .unwrap()
                .to_bytes(),
        ),
    )]));

    let detector = SimulationDetector::new(&rpc, Arc::new(tf), Chain::Ethereum);

    let quality = detector
        .analyze(
            Bytes::from_str("3A9FfF453d50D4Ac52A6890647b823379ba36B9E").unwrap(),
            BlockTag::Latest,
        )
        .await
        .unwrap();

    println!("{quality:?}");
    Ok(())
}
