use alloy::{
    contract::{ContractInstance, Interface},
    dyn_abi::DynSolValue,
    eips::{eip1898::BlockId, BlockNumberOrTag},
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::Block,
    transports::http::reqwest::Url,
};
use miette::{IntoDiagnostic, WrapErr};
use tracing::info;

const NATIVE_ALIASES: &[Address] = &[
    address!("0x0000000000000000000000000000000000000000"),
    address!("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
];

const ERC_20_ABI: &str = r#"[{"inputs":[{"name":"_owner","type":"address"}],"name":"balanceOf","outputs":[{"name":"balance","type":"uint256"}],"stateMutability":"view","type":"function"}]"#;

pub struct RPCProvider {
    pub url: Url,
}

impl RPCProvider {
    pub fn new(url: String) -> Self {
        let url = url.as_str().parse().unwrap();
        RPCProvider { url }
    }

    pub async fn get_token_balance(
        &self,
        token_address: Address,
        wallet_address: Address,
        block_number: u64,
    ) -> miette::Result<U256> {
        let provider = ProviderBuilder::new().connect_http(self.url.clone());
        let block_id: BlockId = BlockId::from(block_number);

        match NATIVE_ALIASES.contains(&token_address) {
            true => provider
                .get_balance(wallet_address)
                .block_id(block_id)
                .await
                .into_diagnostic()
                .wrap_err("Failed to fetch token balance"),
            false => {
                let abi = serde_json::from_str(ERC_20_ABI)
                    .into_diagnostic()
                    .wrap_err("invalid ABI")?;

                let contract = ContractInstance::new(token_address, provider, Interface::new(abi));

                let wallet_addr = DynSolValue::from(wallet_address);

                let result_value = contract
                    .function("balanceOf", &[wallet_addr])
                    .expect("Failed to build function call")
                    .block(block_id)
                    .call()
                    .await
                    .into_diagnostic()
                    .wrap_err("Failed to fetch ERC-20 Balance")?;
                let result: U256 = result_value
                    .first()
                    .ok_or_else(|| miette::miette!("No value returned from contract call"))?
                    .as_uint()
                    .ok_or_else(|| miette::miette!("Returned value is not a uint"))?
                    .0;
                Ok(result)
            }
        }
    }

    pub async fn get_block_header(&self, block_number: u64) -> miette::Result<Block> {
        let provider = ProviderBuilder::new().connect_http(self.url.clone());
        let block_id: BlockId = BlockId::from(block_number);

        provider
            .get_block(block_id)
            .await
            .into_diagnostic()
            .wrap_err("Failed to fetch block header")
            .and_then(|block_opt| block_opt.ok_or_else(|| miette::miette!("Block not found")))
    }

    pub async fn get_block(&self, block_number: BlockNumberOrTag) -> miette::Result<Block> {
        info!("Fetching block {:?}...", block_number.as_number());
        let provider = ProviderBuilder::new().connect_http(self.url.clone());
        provider
            .get_block_by_number(block_number)
            .await
            .into_diagnostic()
            .wrap_err(format!("Failed to fetch block {:?}", block_number))
            .and_then(|block_opt| block_opt.ok_or_else(|| miette::miette!("Block not found")))
    }
}

#[cfg(test)]
mod tests {
    use std::{env, str::FromStr};

    use alloy::primitives::address;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    use super::*;

    #[tokio::test]
    async fn get_token_balance_native_token() {
        let eth_rpc_url = env::var("RPC_URL").expect("Missing RPC_URL in environment");

        let rpc_provider = RPCProvider::new(eth_rpc_url);
        let token_address = address!("0x0000000000000000000000000000000000000000");
        let wallet_address = address!("0x787B8840100d9BaAdD7463f4a73b5BA73B00C6cA");
        let block_number = 21998530;

        let balance = rpc_provider
            .get_token_balance(token_address, wallet_address, block_number)
            .await
            .unwrap();

        assert_eq!(
            balance,
            U256::from_str("1070041574684539264153").expect("Failed to convert ETH value")
        );
    }

    #[tokio::test]
    async fn get_token_balance_erc20_token() {
        let eth_rpc_url = env::var("RPC_URL").expect("Missing RPC_URL in environment");

        let rpc_provider = RPCProvider::new(eth_rpc_url);
        let token_address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let wallet_address = address!("0x787B8840100d9BaAdD7463f4a73b5BA73B00C6cA");
        let block_number = 21998530;

        let balance = rpc_provider
            .get_token_balance(token_address, wallet_address, block_number)
            .await
            .unwrap();

        assert_eq!(balance, U256::from(717250938432_u64));
    }

    #[tokio::test]
    async fn arc_routable_usdc_uses_erc20_balance_of_while_zero_address_uses_native_balance() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock RPC listener must bind");
        let address = listener
            .local_addr()
            .expect("mock RPC listener must have an address");
        let (requests_tx, mut requests_rx) = mpsc::channel(2);

        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("mock RPC must accept request");
                let mut buffer = vec![0; 4096];
                let bytes_read = socket
                    .read(&mut buffer)
                    .await
                    .expect("mock RPC must read request");
                let request = String::from_utf8(buffer[..bytes_read].to_vec())
                    .expect("mock RPC request must be UTF-8");
                let body_start = request
                    .find("\r\n\r\n")
                    .expect("mock RPC request must contain headers") +
                    4;
                let request_id = serde_json::from_str::<serde_json::Value>(&request[body_start..])
                    .expect("mock RPC request must contain JSON")["id"]
                    .clone();
                requests_tx
                    .send(request)
                    .await
                    .expect("test must receive mock RPC request");

                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": "0x000000000000000000000000000000000000000000000000000000000000002a",
                })
                .to_string();
                let http_response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    response.len(),
                    response
                );
                socket
                    .write_all(http_response.as_bytes())
                    .await
                    .expect("mock RPC must respond");
            }
        });

        let rpc_provider = RPCProvider::new(format!("http://{address}"));
        let component = address!("0x1111111111111111111111111111111111111111");

        assert_eq!(
            rpc_provider
                .get_token_balance(
                    address!("0x3600000000000000000000000000000000000000"),
                    component,
                    1,
                )
                .await
                .expect("Arc ERC-20 USDC balance must be readable"),
            U256::from(42)
        );
        assert_eq!(
            rpc_provider
                .get_token_balance(
                    address!("0x0000000000000000000000000000000000000000"),
                    component,
                    1
                )
                .await
                .expect("native balance must be readable"),
            U256::from(42)
        );

        let first_request = requests_rx
            .recv()
            .await
            .expect("first request must be recorded");
        let second_request = requests_rx
            .recv()
            .await
            .expect("second request must be recorded");
        assert!(first_request.contains("eth_call"));
        assert!(first_request.contains("0x3600000000000000000000000000000000000000"));
        assert!(first_request.contains("0x70a08231"));
        assert!(second_request.contains("eth_getBalance"));
    }

    #[tokio::test]
    async fn get_block_header() {
        let eth_rpc_url = env::var("RPC_URL").expect("Missing RPC_URL in environment");

        let rpc_provider = RPCProvider::new(eth_rpc_url);
        let block_number = 21998530;

        let block_header = rpc_provider
            .get_block_header(block_number)
            .await
            .unwrap();

        // Verify that we got a block with the correct number
        assert_eq!(block_number, block_header.header.number);
        assert_eq!(block_header.header.timestamp, 1741393115);
    }
}
