use std::{collections::HashMap, str::FromStr, sync::Arc, time::Instant};

use chrono::NaiveDateTime;
use futures03::{future::try_join_all, FutureExt};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use tycho_common::{
    models::{
        blockchain::BlockTag,
        protocol::QualityRange,
        token::{Token, TokenAnalysis, TokenOwnerStore, TokenVerdict},
        Chain, PaginationParams,
    },
    storage::ProtocolGateway,
    Bytes,
};
use tycho_ethereum::{rpc::EthereumRpcClient, services::token_analyzer::AnalyzerKind};

use crate::cli::AnalyzeTokenArgs;

pub async fn analyze_tokens(
    analyze_args: AnalyzeTokenArgs,
    rpc: &EthereumRpcClient,
    gw: Arc<dyn ProtocolGateway + Send + Sync>,
    analyzer: AnalyzerKind,
) -> anyhow::Result<()> {
    // Skip tokens that failed previously and ones we already analyzed successfully
    run_analysis_pass(
        &analyze_args,
        rpc,
        gw.clone(),
        &analyzer,
        QualityRange::new(6, 10),
        None,
        AnalysisPass::Retry,
    )
    .await?;

    if analyze_args.recovery_lookback_days > 0 {
        // Quality 5 is the analysis floor and 1 marks tokens never to quote: the pass above
        // revisits neither. Re-check those that traded recently — their behavior may have
        // changed since (e.g. launch transfer restrictions lifted), or the verdict was wrong.
        // A blocked verdict keeps them where they are instead of demoting further.
        let traded_since = chrono::Utc::now().naive_utc() -
            chrono::Duration::days(analyze_args.recovery_lookback_days as i64);
        run_analysis_pass(
            &analyze_args,
            rpc,
            gw,
            &analyzer,
            QualityRange::new(1, 5),
            Some(traded_since),
            AnalysisPass::Recovery,
        )
        .await?;
    }

    Ok(())
}

/// The retry pass demotes on a `Blocked`, `TemporarilyBlocked` or `NoLiquidity` verdict so
/// tokens eventually leave the 6–10 retry window. The recovery pass re-checks floored (quality
/// 1–5) tokens that traded recently, which stay where they are on those verdicts.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AnalysisPass {
    Retry,
    Recovery,
}

async fn run_analysis_pass(
    analyze_args: &AnalyzeTokenArgs,
    rpc: &EthereumRpcClient,
    gw: Arc<dyn ProtocolGateway + Send + Sync>,
    analyzer: &AnalyzerKind,
    quality_range: QualityRange,
    traded_since: Option<NaiveDateTime>,
    pass: AnalysisPass,
) -> anyhow::Result<()> {
    // Fetch every matching token before analyzing: analysis promotes tokens out of the
    // quality filter, which shifts OFFSET pages and would skip rows mid-run.
    let mut tokens = Vec::new();
    let mut page = 0;
    let page_size = analyze_args.fetch_batch_size as i64;
    loop {
        let pagination_params = PaginationParams::new(page, page_size);
        let page_tokens = gw
            .get_tokens(
                analyze_args.chain,
                None,
                quality_range.clone(),
                traded_since,
                Some(&pagination_params),
            )
            .await?
            .entity;
        let page_len = page_tokens.len();
        tokens.extend(page_tokens);
        page += 1;
        if page_len < (page_size as usize) {
            break;
        }
    }
    info!(matched = tokens.len(), ?pass, "Starting analysis pass");

    let start = Instant::now();
    let sem = Arc::new(Semaphore::new(analyze_args.concurrency));
    let tasks = tokens
        .chunks(analyze_args.update_batch_size)
        .map(|chunk| {
            analyze_batch(
                analyze_args.chain,
                rpc,
                chunk.to_vec(),
                sem.clone(),
                gw.clone(),
                analyzer,
                pass,
            )
            .boxed()
        })
        .collect::<Vec<_>>();

    let outcomes = try_join_all(tasks).await?;
    let mut totals = PassOutcome::default();
    for outcome in outcomes {
        totals.promoted += outcome.promoted;
        totals.demoted += outcome.demoted;
        totals.unchanged += outcome.unchanged;
        totals.failed += outcome.failed;
    }
    let duration = Instant::now().duration_since(start);
    info!(
        ?pass,
        analyzed = tokens.len(),
        promoted = totals.promoted,
        demoted = totals.demoted,
        unchanged = totals.unchanged,
        failed = totals.failed,
        duration = duration.as_secs(),
        "Analysis pass complete"
    );

    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct PassOutcome {
    promoted: usize,
    demoted: usize,
    unchanged: usize,
    failed: usize,
}

async fn analyze_batch(
    chain: Chain,
    rpc: &EthereumRpcClient,
    mut tokens: Vec<Token>,
    sem: Arc<Semaphore>,
    gw: Arc<dyn ProtocolGateway + Send + Sync>,
    analyzer: &AnalyzerKind,
    pass: AnalysisPass,
) -> anyhow::Result<PassOutcome> {
    let _guard = sem.acquire().await?;
    let addresses = tokens
        .iter()
        .map(|t| t.address.clone())
        .collect::<Vec<_>>();
    let token_owner = gw
        .get_token_owners(&chain, &addresses, Some(100_000f64))
        .await?;
    let component_ids = token_owner
        .values()
        .map(|(cid, _)| cid.as_str())
        .collect::<Vec<_>>();
    let components = gw
        .get_protocol_components(&chain, None, Some(&component_ids), None, None)
        .await?
        .entity
        .into_iter()
        .map(|pc| (pc.id.clone(), pc))
        .collect::<HashMap<_, _>>();

    let balance_owners = gw
        .get_protocol_states(&chain, None, None, Some(&component_ids), false, None)
        .await?
        .entity
        .into_iter()
        .filter_map(|state| {
            state
                .attributes
                .get("balance_owner")
                .cloned()
                .map(|owner| (state.component_id.clone(), owner))
        })
        .collect::<HashMap<_, _>>();

    let liquidity_token_owners = token_owner
        .into_iter()
        .filter_map(|(address, (cid, balance))| {
            if let Some(pc) = components.get(&cid) {
                let liq_owner = balance_owners
                    .get(&pc.id)
                    .cloned()
                    .or_else(|| {
                        pc.contract_addresses
                            // TODO: Currently, it's assumed that the pool is always the first
                            // contract in the protocol component. This approach is a temporary
                            // workaround and needs to be revisited for a more robust solution.
                            .first()
                            .cloned()
                            .or_else(|| Bytes::from_str(&pc.id).ok())
                    });

                liq_owner.map(|liq_owner| (address, (liq_owner, balance)))
            } else {
                warn!(component_id=?cid, "Failed to find component for id!");
                None
            }
        })
        .collect::<HashMap<_, _>>();
    let analyzer = analyzer.build(rpc, Arc::new(TokenOwnerStore::new(liquidity_token_owners)));
    let mut outcome = PassOutcome::default();
    let mut analyses = Vec::with_capacity(tokens.len());
    for t in tokens.iter_mut() {
        debug!(?t.address, "Analyzing token");
        let analysis = match analyzer
            .analyze(t.address.clone(), BlockTag::Latest)
            .await
        {
            Ok(analysis) => analysis,
            Err(error) => {
                warn!(?t.address, ?error, "Token quality detection failed");
                outcome.failed += 1;
                continue;
            }
        };

        let quality_before = t.quality;
        apply_analysis(t, &analysis, pass);
        analyses.push((t.address.clone(), analysis));
        if t.quality > quality_before {
            outcome.promoted += 1;
        } else if t.quality < quality_before {
            outcome.demoted += 1;
        } else {
            outcome.unchanged += 1;
        }
    }

    if !tokens.is_empty() {
        gw.update_tokens(&tokens).await?;
    }
    if !analyses.is_empty() {
        gw.upsert_token_analyses(&analyses)
            .await?;
    }
    Ok(outcome)
}

/// Sets the token's quality, tax and gas from `analysis`.
///
/// Quality follows the verdict, except for `Blocked`, `TemporarilyBlocked` and `NoLiquidity`:
/// the retry pass lowers quality by one per attempt so the token eventually leaves the 6–10
/// retry window, and the recovery pass keeps floored tokens where they are. A token without a
/// pool, or one whose owner never opens it, would otherwise be retried on every run; the
/// recovery pass re-checks it once it trades.
fn apply_analysis(t: &mut Token, analysis: &TokenAnalysis, pass: AnalysisPass) {
    let quality_before = t.quality;
    t.apply_analysis(analysis);
    match analysis.verdict {
        TokenVerdict::Blocked | TokenVerdict::TemporarilyBlocked | TokenVerdict::NoLiquidity => {
            debug!(?t.address, reason = ?analysis.reason, "Token quality detected as bad!");
            t.quality = match pass {
                AnalysisPass::Retry => quality_before.saturating_sub(1),
                AnalysisPass::Recovery => quality_before,
            };
        }
        TokenVerdict::Ok | TokenVerdict::Fee | TokenVerdict::NoQuote => {}
    }
}

#[cfg(test)]
mod test {
    use chrono::NaiveDateTime;
    use mockall::Sequence;
    use rstest::rstest;
    use tycho_common::{
        models::{protocol::ProtocolComponent, ChangeType},
        storage::WithTotal,
    };

    use super::*;
    use crate::{cli::TokenAnalyzerArgs, testing};

    fn test_token(quality: u32) -> Token {
        Token::new(
            &Bytes::from("0xe172e9b6cfbeeb5593bdce3f077356fdb33af904"),
            "FOLD",
            18,
            0,
            &[],
            Chain::Ethereum,
            quality,
        )
    }

    fn settlement_contract() -> alloy::primitives::Address {
        "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58"
            .parse()
            .unwrap()
    }

    fn token_analyzer_args() -> TokenAnalyzerArgs {
        TokenAnalyzerArgs { settlement_contract: settlement_contract(), token_watchlist: vec![] }
    }

    fn eth_call_analyzer() -> AnalyzerKind {
        AnalyzerKind::EthCall { settlement_contract: settlement_contract() }
    }

    fn test_token_at(address: &str, quality: u32) -> Token {
        Token::new(&Bytes::from(address), "TEST", 18, 0, &[], Chain::Ethereum, quality)
    }

    fn analysis(verdict: TokenVerdict, transfer_tax: i64) -> TokenAnalysis {
        let mut analysis = TokenAnalysis::verdict_only(verdict, None, None, "test");
        analysis.fees.transfer = Some(transfer_tax);
        analysis.gas = Some(30_000);
        analysis
    }

    #[rstest]
    #[case::ok_promotes(TokenVerdict::Ok, 8, AnalysisPass::Retry, 100)]
    #[case::ok_promotes_in_recovery_pass(TokenVerdict::Ok, 5, AnalysisPass::Recovery, 100)]
    #[case::blocked_demotes(TokenVerdict::Blocked, 8, AnalysisPass::Retry, 7)]
    #[case::blocked_keeps_quality_in_recovery_pass(
        TokenVerdict::Blocked,
        5,
        AnalysisPass::Recovery,
        5
    )]
    #[case::no_liquidity_demotes(TokenVerdict::NoLiquidity, 8, AnalysisPass::Retry, 7)]
    #[case::no_liquidity_keeps_quality_in_recovery_pass(
        TokenVerdict::NoLiquidity,
        5,
        AnalysisPass::Recovery,
        5
    )]
    #[case::temporarily_blocked_demotes(
        TokenVerdict::TemporarilyBlocked,
        8,
        AnalysisPass::Retry,
        7
    )]
    #[case::temporarily_blocked_keeps_quality_in_recovery_pass(
        TokenVerdict::TemporarilyBlocked,
        5,
        AnalysisPass::Recovery,
        5
    )]
    #[case::no_quote_leaves_retry_window(TokenVerdict::NoQuote, 8, AnalysisPass::Retry, 1)]
    #[case::no_quote_recovers_in_recovery_pass(TokenVerdict::Ok, 1, AnalysisPass::Recovery, 100)]
    fn test_apply_analysis_quality(
        #[case] verdict: TokenVerdict,
        #[case] initial_quality: u32,
        #[case] pass: AnalysisPass,
        #[case] expected_quality: u32,
    ) {
        let mut t = test_token(initial_quality);

        apply_analysis(&mut t, &analysis(verdict, 0), pass);

        assert_eq!(t.quality, expected_quality);
        assert_eq!(t.gas, vec![Some(30_000)]);
        assert_eq!(t.tax, 0);
    }

    #[rstest]
    #[case::retry_pass(AnalysisPass::Retry)]
    #[case::recovery_pass(AnalysisPass::Recovery)]
    fn test_apply_analysis_fee_token_sets_50(#[case] pass: AnalysisPass) {
        let mut t = test_token(5);

        apply_analysis(&mut t, &analysis(TokenVerdict::Fee, 250), pass);

        assert_eq!(t.quality, 50);
        assert_eq!(t.tax, 250);
    }

    #[test_log::test(tokio::test)]
    async fn test_pass_fetches_all_pages_before_analyzing() {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("url parses");
        let args = AnalyzeTokenArgs {
            chain: Chain::Ethereum,
            token_analyzer: token_analyzer_args(),
            concurrency: 1,
            update_batch_size: 2,
            fetch_batch_size: 2,
            recovery_lookback_days: 0,
        };
        let mut seq = Sequence::new();
        let mut gw = testing::MockGateway::new();
        gw.expect_get_tokens()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _, _, _| {
                Box::pin(async {
                    Ok(WithTotal {
                        entity: vec![
                            test_token_at("0x0000000000000000000000000000000000000001", 8),
                            test_token_at("0x0000000000000000000000000000000000000002", 8),
                        ],
                        total: Some(2),
                    })
                })
            });
        gw.expect_get_tokens()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(2) }) })
            });
        gw.expect_get_token_owners()
            .returning(|_, _, _| Box::pin(async { Ok(HashMap::new()) }));
        gw.expect_get_protocol_components()
            .returning(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });
        gw.expect_get_protocol_states()
            .returning(|_, _, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });
        gw.expect_update_tokens()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Box::pin(async { Ok(()) }));
        gw.expect_upsert_token_analyses()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|analyses| {
                assert_eq!(analyses.len(), 2);
                Box::pin(async { Ok(()) })
            });

        analyze_tokens(args, &rpc, Arc::new(gw), eth_call_analyzer())
            .await
            .expect("analyze tokens failed");
    }

    fn wiring_args(recovery_lookback_days: u32) -> AnalyzeTokenArgs {
        AnalyzeTokenArgs {
            chain: Chain::Ethereum,
            token_analyzer: token_analyzer_args(),
            concurrency: 1,
            update_batch_size: 10,
            fetch_batch_size: 10,
            recovery_lookback_days,
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_analyze_tokens_runs_recovery_pass_after_retry_pass() {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("url parses");
        let mut seq = Sequence::new();
        let mut gw = testing::MockGateway::new();
        gw.expect_get_tokens()
            .withf(|_, _, quality, traded_since, _| {
                quality.min == Some(6) && quality.max == Some(10) && traded_since.is_none()
            })
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });
        gw.expect_get_tokens()
            .withf(|_, _, quality, traded_since, _| {
                quality.min == Some(1) && quality.max == Some(5) && traded_since.is_some()
            })
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });

        analyze_tokens(wiring_args(1), &rpc, Arc::new(gw), eth_call_analyzer())
            .await
            .expect("analyze tokens failed");
    }

    #[test_log::test(tokio::test)]
    async fn test_recovery_pass_disabled_at_zero() {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("url parses");
        let mut gw = testing::MockGateway::new();
        gw.expect_get_tokens()
            .withf(|_, _, quality, traded_since, _| {
                quality.min == Some(6) && quality.max == Some(10) && traded_since.is_none()
            })
            .times(1)
            .returning(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });

        analyze_tokens(wiring_args(0), &rpc, Arc::new(gw), eth_call_analyzer())
            .await
            .expect("analyze tokens failed");
    }

    fn outcome_test_gateway() -> testing::MockGateway {
        let mut gw = testing::MockGateway::new();
        gw.expect_get_token_owners()
            .returning(|_, _, _| Box::pin(async { Ok(HashMap::new()) }));
        gw.expect_get_protocol_components()
            .returning(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });
        gw.expect_get_protocol_states()
            .returning(|_, _, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });
        gw.expect_update_tokens()
            .times(1)
            .returning(|_| Box::pin(async { Ok(()) }));
        gw.expect_upsert_token_analyses()
            .times(1)
            .returning(|_| Box::pin(async { Ok(()) }));
        gw
    }

    async fn run_outcome_batch(pass: AnalysisPass, initial_quality: u32) -> PassOutcome {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("url parses");
        let tokens = vec![
            test_token_at("0x0000000000000000000000000000000000000001", initial_quality),
            test_token_at("0x0000000000000000000000000000000000000002", initial_quality),
        ];
        analyze_batch(
            Chain::Ethereum,
            &rpc,
            tokens,
            Arc::new(Semaphore::new(1)),
            Arc::new(outcome_test_gateway()),
            &eth_call_analyzer(),
            pass,
        )
        .await
        .expect("analyze batch failed")
    }

    #[test_log::test(tokio::test)]
    async fn test_retry_pass_counts_demotions() {
        // No owner in the store -> analyzer returns Bad without RPC -> retry pass demotes.
        let outcome = run_outcome_batch(AnalysisPass::Retry, 8).await;
        assert_eq!(outcome, PassOutcome { demoted: 2, ..Default::default() });
    }

    #[test_log::test(tokio::test)]
    async fn test_recovery_pass_counts_unchanged() {
        let outcome = run_outcome_batch(AnalysisPass::Recovery, 5).await;
        assert_eq!(outcome, PassOutcome { unchanged: 2, ..Default::default() });
    }

    // requires a running ethereum node
    #[ignore = "require RPC connection"]
    #[test_log::test(tokio::test)]
    async fn test_analyze_tokens() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC URL must be set for testing");
        let rpc = EthereumRpcClient::new(&rpc_url).expect("failed to create rpc client");

        let args = AnalyzeTokenArgs {
            chain: Chain::Ethereum,
            token_analyzer: token_analyzer_args(),
            concurrency: 10,
            update_batch_size: 100,
            fetch_batch_size: 100,
            recovery_lookback_days: 0,
        };
        let mut gw = testing::MockGateway::new();
        gw.expect_get_tokens()
            .returning(|_, _, _, _, _| {
                Box::pin(async {
                    Ok(WithTotal {
                        entity: vec![
                            Token::new(
                                &Bytes::from("0x228c6fcd7376177ff0cff304043f461189752750"),
                                "BLITZ",
                                9,
                                0,
                                &[],
                                Chain::Ethereum,
                                10,
                            ),
                            Token::new(
                                &Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                                "WETH",
                                18,
                                0,
                                &[],
                                Chain::Ethereum,
                                10,
                            ),
                        ],
                        total: Some(2),
                    })
                })
            });
        let exp = vec![
            Token::new(
                &Bytes::from("0x228c6fcd7376177ff0cff304043f461189752750"),
                "BLITZ",
                9,
                500,
                &[Some(66_960)],
                Chain::Ethereum,
                50,
            ),
            Token::new(
                &Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                "WETH",
                18,
                0,
                &[Some(29_962)],
                Chain::Ethereum,
                100,
            ),
        ];
        gw.expect_get_token_owners()
            .returning(|_, _, _| {
                Box::pin(async move {
                    Ok(HashMap::from([
                        (
                            Bytes::from("0x228c6fcd7376177ff0cff304043f461189752750"),
                            (
                                "0x7ec8e94a9b379f6b90ee5af7b9a78624280b50ea".to_string(),
                                Bytes::from("0x0186a0"),
                            ),
                        ),
                        (
                            Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                            (
                                "0x7ec8e94a9b379f6b90ee5af7b9a78624280b50ea".to_string(),
                                Bytes::from("0x0186a0"),
                            ),
                        ),
                    ]))
                })
            });
        gw.expect_get_protocol_components()
            .returning(|_, _, _, _, _| {
                Box::pin(async move {
                    Ok(WithTotal {
                        entity: vec![ProtocolComponent::new(
                            "0x7ec8e94a9b379f6b90ee5af7b9a78624280b50ea",
                            "uniswap_v2",
                            "pool",
                            Chain::Ethereum,
                            vec![
                                Bytes::from("0x228c6fcd7376177ff0cff304043f461189752750"),
                                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                            ],
                            vec![],
                            HashMap::new(),
                            ChangeType::Creation,
                            Bytes::from("0x00"),
                            NaiveDateTime::default(),
                        )],
                        total: Some(1),
                    })
                })
            });
        gw.expect_update_tokens()
            .once()
            .returning(move |updated| {
                assert_eq!(updated, &exp);
                Box::pin(async { Ok(()) })
            });
        gw.expect_upsert_token_analyses()
            .once()
            .returning(|analyses| {
                assert_eq!(analyses.len(), 2);
                Box::pin(async { Ok(()) })
            });

        analyze_tokens(args, &rpc, Arc::new(gw), eth_call_analyzer())
            .await
            .expect("analyze tokens failed");
    }
}
