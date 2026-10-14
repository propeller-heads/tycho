//! Turning streamed books into the directional quote ladders the stream serves.
//!
//! A book prices one pair in human units and names its tokens by symbol, so pricing it takes two
//! steps: resolving the symbols to the consumer's tokens through a trusted binding
//! ([`resolve_book_tokens`]), and converting both sides of the book into base-unit ladders
//! ([`ladders_for_book`]).

use std::collections::HashMap;

use num_bigint::BigUint;
use num_traits::FromPrimitive;
use tycho_common::{models::token::Token, Bytes};

use super::{
    config::SymbolBindings,
    state::PriceLevelStreamQuote,
    titan::{TitanBook, TitanLevel},
};

/// Why a book's symbols did not resolve to a pair of admitted tokens. Both variants name the
/// symbol that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnresolvedSymbol<'a> {
    /// No binding names the symbol (`unbound_symbol`), so nothing authenticates an address for
    /// it — a token in the consumer's map that carries the symbol is not evidence.
    Unbound(&'a str),
    /// The symbol is bound, but the consumer's token map does not carry the bound address
    /// (`unadmitted_token`): the consumer does not admit the asset, and nothing supplies the
    /// decimals its amounts are measured in.
    Unadmitted(&'a str),
}

/// Resolves the `token1`/`token2` symbols of `book` to the tokens its levels measure, in that
/// order.
///
/// A symbol resolves only through `bindings`, and the bound address must be present in `tokens`;
/// that admitted token supplies the decimals the conversion needs. Resolution never reads the
/// symbols of `tokens`: a token that calls itself `WBTC` neither captures the bound asset's
/// quotes nor keeps the bound asset from being served. Both failures leave the book unpriceable.
pub(super) fn resolve_book_tokens<'book, 'tokens>(
    book: &'book TitanBook,
    bindings: &SymbolBindings,
    tokens: &'tokens HashMap<Bytes, Token>,
) -> Result<(&'tokens Token, &'tokens Token), UnresolvedSymbol<'book>> {
    Ok((
        resolve_symbol(&book.token1, bindings, tokens)?,
        resolve_symbol(&book.token2, bindings, tokens)?,
    ))
}

fn resolve_symbol<'book, 'tokens>(
    symbol: &'book str,
    bindings: &SymbolBindings,
    tokens: &'tokens HashMap<Bytes, Token>,
) -> Result<&'tokens Token, UnresolvedSymbol<'book>> {
    let Some(address) = bindings.address(symbol) else {
        return Err(UnresolvedSymbol::Unbound(symbol));
    };
    tokens
        .get(address)
        .ok_or(UnresolvedSymbol::Unadmitted(symbol))
}

/// Why a whole book side was rejected.
///
/// A corrupt side is rejected, never sanitized: the ladder interpolates between its knots, so
/// dropping the offending level would turn a broken price function into an invented usable one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SideRejection {
    /// A level's streamed `price` or `cum_volume` is not a finite positive number, so the level
    /// states no price at all.
    UnsupportedLevel,
    /// A level's amount overflowed `f64` to infinity, leaving its size unknown.
    NonFiniteAmount,
    /// A level rounds to a zero `amount_in` or `amount_out`: a knot at zero is not a quote, and
    /// interpolating across it fabricates depth the venue never offered.
    ZeroAmount,
    /// Two levels round to the same `amount_in` but disagree on `amount_out`, so the side is not
    /// a function of the input amount.
    ConflictingKnots,
}

/// One direction's quote ladder, or why the side it came from was rejected. An accepted side
/// that carried no levels yields an empty ladder: that direction is simply unquotable.
pub(super) type Ladder = Result<Vec<PriceLevelStreamQuote>, SideRejection>;

/// Which of the book's tokens the levels of a side sell.
#[derive(Debug, Clone, Copy)]
enum Side {
    /// The bids: a taker sells `token1` into them, so `cum_volume` is the input amount.
    Bids,
    /// The asks: a taker sells `token2` into them, so `price · cum_volume` is the input amount.
    Asks,
}

/// The quote ladders of `book` as `(token1 → token2, token2 → token1)`, in base units of the
/// resolved tokens and in stream order.
///
/// Reads `price` as the average execution price of the level's cumulative size, in `token2` per
/// `token1`, and `cum_volume` as whole `token1`. The bids are therefore the ladder selling
/// `token1` and the asks the ladder selling `token2`:
///
/// ```text
/// bids: amount_in = round(cum_volume · 10^dec1), amount_out = round(price · cum_volume · 10^dec2)
/// asks: amount_in = round(price · cum_volume · 10^dec2), amount_out = round(cum_volume · 10^dec1)
/// ```
///
/// Amounts are rounded to the nearest base unit. Their precision is Titan's own, since Titan
/// quantized both values to `f64` before sending them, and no tighter bound is claimed. The two
/// sides are converted independently, so a rejected side never costs the other one.
pub(super) fn ladders_for_book(
    book: &TitanBook,
    token1: &Token,
    token2: &Token,
) -> (Ladder, Ladder) {
    (ladder(&book.bids, token1, token2, Side::Bids), ladder(&book.asks, token1, token2, Side::Asks))
}

fn ladder(levels: &[TitanLevel], token1: &Token, token2: &Token, side: Side) -> Ladder {
    let mut quotes = Vec::with_capacity(levels.len());
    for level in levels {
        ensure_supported(level)?;
        let token1_amount = base_units(level.cum_volume * pow10(token1.decimals))?;
        let token2_amount = base_units(level.price * level.cum_volume * pow10(token2.decimals))?;
        let (amount_in, amount_out) = match side {
            Side::Bids => (token1_amount, token2_amount),
            Side::Asks => (token2_amount, token1_amount),
        };
        quotes.push(PriceLevelStreamQuote::new(amount_in, amount_out));
    }
    ensure_single_valued(&quotes)?;
    Ok(quotes)
}

/// Errors unless both streamed values of `level` are finite and positive.
fn ensure_supported(level: &TitanLevel) -> Result<(), SideRejection> {
    for value in [level.price, level.cum_volume] {
        if !value.is_finite() || value <= 0.0 {
            return Err(SideRejection::UnsupportedLevel);
        }
    }
    Ok(())
}

/// Errors if two quotes round to the same `amount_in` but disagree on `amount_out`. Levels that
/// repeat a knot are not a conflict: Titan repeats a cumulative volume across levels.
fn ensure_single_valued(quotes: &[PriceLevelStreamQuote]) -> Result<(), SideRejection> {
    let mut outputs: HashMap<&BigUint, &BigUint> = HashMap::with_capacity(quotes.len());
    for quote in quotes {
        let previous = outputs.insert(&quote.amount_in, &quote.amount_out);
        if previous.is_some_and(|previous| previous != &quote.amount_out) {
            return Err(SideRejection::ConflictingKnots);
        }
    }
    Ok(())
}

/// `product` rounded to the nearest whole base unit.
fn base_units(product: f64) -> Result<BigUint, SideRejection> {
    // `from_f64` decomposes the float's mantissa and binary exponent, so amounts above
    // `u64::MAX` — every 18-decimal amount from ~18.5 tokens up — convert exactly instead of
    // saturating, and it returns `None` for exactly what has no integer value.
    let Some(amount) = BigUint::from_f64(product.round()) else {
        return Err(SideRejection::NonFiniteAmount);
    };
    if amount == BigUint::ZERO {
        return Err(SideRejection::ZeroAmount);
    }
    Ok(amount)
}

/// `10^decimals`, exact for every decimals up to 22. Beyond that the power itself is rounded,
/// which only compounds the quantization Titan already applied; an absurd decimals overflows
/// to infinity and rejects the side.
fn pow10(decimals: u32) -> f64 {
    10f64.powi(i32::try_from(decimals).unwrap_or(i32::MAX))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::{
        super::{
            config::SYMBOL_BINDINGS,
            test_support::{captured_book, captured_books, token, PAMM, USDC, WBTC, WETH},
            titan::Provenance,
        },
        *,
    };

    fn level(price: f64, cum_volume: f64) -> TitanLevel {
        TitanLevel { price, cum_volume }
    }

    fn quote(amount_in: u64, amount_out: u64) -> PriceLevelStreamQuote {
        PriceLevelStreamQuote::new(BigUint::from(amount_in), BigUint::from(amount_out))
    }

    fn big(decimal: &str) -> BigUint {
        BigUint::parse_bytes(decimal.as_bytes(), 10).expect("decimal amount")
    }

    /// The address [`SYMBOL_BINDINGS`] binds `symbol` to.
    fn bound_address(symbol: &str) -> Bytes {
        SymbolBindings::default()
            .address(symbol)
            .expect("bound symbol")
            .clone()
    }

    /// The bound token of `symbol`, as a consumer admitting it with `decimals`.
    fn bound_token(symbol: &str, decimals: u32) -> Token {
        token(&bound_address(symbol).to_string(), symbol, decimals)
    }

    fn wbtc() -> Token {
        bound_token("WBTC", 8)
    }

    fn usdc() -> Token {
        bound_token("USDC", 6)
    }

    fn weth() -> Token {
        bound_token("WETH", 18)
    }

    /// Synthetic: a token that calls itself `WBTC` at an address no binding names.
    fn impostor_wbtc() -> Token {
        token("0x00000000000000000000000000000000deadbeef", "WBTC", 18)
    }

    fn token_map(tokens: impl IntoIterator<Item = Token>) -> HashMap<Bytes, Token> {
        tokens
            .into_iter()
            .map(|token| (token.address.clone(), token))
            .collect()
    }

    /// Every bound asset with the decimals of its Ethereum deployment, as a consumer token map.
    fn bound_tokens() -> HashMap<Bytes, Token> {
        token_map([wbtc(), usdc(), weth(), bound_token("USDT", 6), bound_token("cbBTC", 8)])
    }

    /// Synthetic: a WBTC/USDC book of the Fermi venue carrying the given sides.
    fn synthetic_book(bids: Vec<TitanLevel>, asks: Vec<TitanLevel>) -> TitanBook {
        TitanBook {
            maker: Some("synthetic".to_string()),
            quote_address: Bytes::from_str(PAMM).expect("venue address"),
            token1: "WBTC".to_string(),
            token2: "USDC".to_string(),
            calculated_from: Provenance::Absent,
            asks,
            bids,
        }
    }

    /// The ladder must be a usable price function: every knot a positive amount, strictly
    /// increasing in `amount_in` and non-decreasing in `amount_out`.
    fn assert_usable(quotes: &[PriceLevelStreamQuote], label: &str) {
        for quote in quotes {
            assert!(quote.amount_in > BigUint::ZERO, "{label}: zero amount_in");
            assert!(quote.amount_out > BigUint::ZERO, "{label}: zero amount_out");
        }
        for knots in quotes.windows(2) {
            assert!(knots[0].amount_in < knots[1].amount_in, "{label}: {knots:?} repeat an input");
            assert!(knots[0].amount_out <= knots[1].amount_out, "{label}: {knots:?} lose output");
        }
    }

    #[test]
    fn converts_the_measured_fermi_wbtc_usdc_book() {
        // Measured 2026-10-08, 1 ms skew vs the V1 stream (analysis.md):
        let book = TitanBook {
            maker: Some("Fermi".to_string()),
            quote_address: Bytes::from_str(PAMM).expect("venue address"),
            token1: "WBTC".to_string(),
            token2: "USDC".to_string(),
            calculated_from: Provenance::Absent,
            asks: vec![level(80801.55138978669, 0.00012376), level(80800.59272606572, 0.00021089)],
            bids: vec![level(80773.61600000001, 0.0005), level(80773.6106, 0.005)],
        };

        let (sell_wbtc, sell_usdc) = ladders_for_book(&book, &wbtc(), &usdc());

        // bids: 0.0005 WBTC -> 40.386808 USDC (80773.616 · 0.0005), in base units.
        let sell_wbtc = sell_wbtc.expect("the bids convert");
        assert_eq!(sell_wbtc[0], quote(50_000, 40_386_808));
        assert_eq!(sell_wbtc[1], quote(500_000, 403_868_053));
        // asks: 10.000000 USDC -> 0.00012376 WBTC (price · cumVolume = amountIn).
        let sell_usdc = sell_usdc.expect("the asks convert");
        assert_eq!(sell_usdc[0], quote(10_000_000, 12_376));
        assert_eq!(sell_usdc[1], quote(17_040_037, 21_089));
    }

    #[test]
    fn converts_the_captured_fermi_wbtc_usdc_book() {
        let book = captured_book("Fermi", "WBTC", "USDC");

        let (sell_wbtc, sell_usdc) = ladders_for_book(&book, &wbtc(), &usdc());

        // bids[0]: 0.0005 WBTC at 80970.38 USDC -> 40.485190 USDC.
        let sell_wbtc = sell_wbtc.expect("the captured bids convert");
        assert_eq!(sell_wbtc.len(), 4);
        assert_eq!(sell_wbtc[0], quote(50_000, 40_485_190));
        // asks[0]: 80997.89405475458 · 0.00012346 = 10.000000 USDC -> 0.00012346 WBTC.
        let sell_usdc = sell_usdc.expect("the captured asks convert");
        assert_eq!(sell_usdc.len(), 24);
        assert_eq!(sell_usdc[0], quote(10_000_000, 12_346));
    }

    /// The deepest captured Fermi WETH/USDC bid is 916.49 WETH, about 50 times `u64::MAX` in
    /// base units: narrowing the product through a `u64` would saturate it.
    #[test]
    fn converts_amounts_beyond_u64_exactly() {
        let book = captured_book("Fermi", "WETH", "USDC");

        let (sell_weth, _) = ladders_for_book(&book, &weth(), &usdc());

        let sell_weth = sell_weth.expect("the captured bids convert");
        let deepest = sell_weth
            .last()
            .expect("the book is two-sided");
        assert!(deepest.amount_in > BigUint::from(u64::MAX));
        assert_eq!(deepest.amount_in, big("916490355416216371200"));
        assert_eq!(deepest.amount_out, BigUint::from(1_498_424_999_995u64));
    }

    /// Every book the bindings name converts, and only the deliberately unbound `NVDAx` book is
    /// skipped.
    #[test]
    fn converts_every_bound_book_of_the_capture() {
        let bindings = SymbolBindings::default();
        let tokens = bound_tokens();
        let books = captured_books();
        assert_eq!(books.len(), 20);

        let mut converted = 0;
        let mut unbound = Vec::new();
        for book in &books {
            let pair = format!("{}/{}", book.token1, book.token2);
            let (token1, token2) = match resolve_book_tokens(book, &bindings, &tokens) {
                Ok(tokens) => tokens,
                Err(UnresolvedSymbol::Unbound(symbol)) => {
                    unbound.push(symbol);
                    continue;
                }
                Err(UnresolvedSymbol::Unadmitted(symbol)) => {
                    panic!("{pair}: the bound {symbol} is admitted but did not resolve")
                }
            };

            let (sell_token1, sell_token2) = ladders_for_book(book, token1, token2);

            for (side, ladder) in [("bids", sell_token1), ("asks", sell_token2)] {
                let quotes = ladder
                    .unwrap_or_else(|rejection| panic!("{pair} {side} rejected: {rejection:?}"));
                assert_usable(&quotes, &format!("{pair} {side}"));
            }
            converted += 1;
        }

        assert_eq!(unbound, ["NVDAx"]);
        assert_eq!(converted, 19);
    }

    /// Titan quoted Bebop's captured WBTC/USDC book one-sided: the empty side converts to an
    /// empty ladder, leaving that direction unquotable as a missing V1 pair direction did.
    #[test]
    fn an_empty_side_leaves_its_direction_unquotable() {
        let book = captured_book("Bebop", "WBTC", "USDC");
        assert!(book.asks.is_empty());

        let (sell_wbtc, sell_usdc) = ladders_for_book(&book, &wbtc(), &usdc());

        assert_eq!(sell_usdc, Ok(Vec::new()));
        assert_eq!(
            sell_wbtc
                .expect("the captured bids convert")
                .len(),
            3
        );
    }

    #[test]
    fn binds_only_the_independently_resolved_symbols() {
        let symbols: Vec<&str> = SYMBOL_BINDINGS
            .iter()
            .map(|(symbol, _)| *symbol)
            .collect();
        assert_eq!(symbols, ["WETH", "USDC", "USDT", "WBTC", "cbBTC"]);

        let bindings = SymbolBindings::default();
        assert_eq!(bindings.address("WBTC"), Some(&Bytes::from_str(WBTC).expect("address")));
        assert_eq!(bindings.address("USDC"), Some(&Bytes::from_str(USDC).expect("address")));
        assert_eq!(bindings.address("WETH"), Some(&Bytes::from_str(WETH).expect("address")));
        // Streamed, but deliberately unbound until its address is independently resolved.
        assert_eq!(bindings.address("NVDAx"), None);
        // Matching is exact, so a symbol differing in case stays unbound.
        assert_eq!(bindings.address("wbtc"), None);
    }

    #[test]
    fn caller_bindings_win_over_the_defaults() {
        let nvdax = Bytes::from_str("0x000000000000000000000000000000000000beef").expect("address");
        let bindings = SymbolBindings::with_overrides(HashMap::from([
            ("WBTC".to_string(), impostor_wbtc().address),
            ("NVDAx".to_string(), nvdax.clone()),
        ]));

        assert_eq!(bindings.address("WBTC"), Some(&impostor_wbtc().address));
        assert_eq!(bindings.address("NVDAx"), Some(&nvdax));
        assert_eq!(bindings.address("USDC"), Some(&Bytes::from_str(USDC).expect("address")));
    }

    /// A symbol resolves through its binding, never through the consumer's own token symbols:
    /// the synthetic impostor `WBTC` must not capture the bound asset's quotes, and its presence
    /// must not keep the bound asset from being served either.
    #[rstest]
    #[case::canonical_only(vec![wbtc(), usdc()], Some(WBTC))]
    #[case::impostor_only(vec![impostor_wbtc(), usdc()], None)]
    #[case::both_present(vec![wbtc(), impostor_wbtc(), usdc()], Some(WBTC))]
    fn resolves_symbols_through_the_binding_only(
        #[case] admitted: Vec<Token>,
        #[case] expected: Option<&str>,
    ) {
        let book = captured_book("Fermi", "WBTC", "USDC");
        let tokens = token_map(admitted);

        let resolved = resolve_book_tokens(&book, &SymbolBindings::default(), &tokens);

        let Some(expected) = expected else {
            assert_eq!(resolved.err(), Some(UnresolvedSymbol::Unadmitted("WBTC")));
            return;
        };
        let (token1, token2) = resolved.expect("the bound WBTC is admitted");
        assert_eq!(token1.address, Bytes::from_str(expected).expect("address"));
        assert_eq!(token2.address, Bytes::from_str(USDC).expect("address"));
        // The bound token supplies the decimals, so the impostor's 18 never reach the amounts.
        let (sell_wbtc, _) = ladders_for_book(&book, token1, token2);
        assert_eq!(sell_wbtc.expect("the captured bids convert")[0], quote(50_000, 40_485_190));
    }

    #[test]
    fn unbound_symbols_fail_closed() {
        let book = captured_book("TaurusFi", "NVDAx", "USDC");

        let tokens = bound_tokens();

        let resolved = resolve_book_tokens(&book, &SymbolBindings::default(), &tokens);

        assert_eq!(resolved.err(), Some(UnresolvedSymbol::Unbound("NVDAx")));
    }

    #[test]
    fn symbols_the_consumer_does_not_admit_fail_closed() {
        let book = captured_book("Fermi", "WBTC", "USDT");
        let tokens = token_map([wbtc(), usdc()]);

        let resolved = resolve_book_tokens(&book, &SymbolBindings::default(), &tokens);

        assert_eq!(resolved.err(), Some(UnresolvedSymbol::Unadmitted("USDT")));
    }

    /// Synthetic: a level whose price or size is not a finite positive number states no price,
    /// and the side it sits in cannot be repaired by dropping it. The valid opposite side is
    /// unaffected.
    #[rstest]
    #[case::nan_price(f64::NAN, 0.0005)]
    #[case::infinite_price(f64::INFINITY, 0.0005)]
    #[case::zero_price(0.0, 0.0005)]
    #[case::negative_price(-80_773.616, 0.0005)]
    #[case::nan_volume(80_773.616, f64::NAN)]
    #[case::zero_volume(80_773.616, 0.0)]
    #[case::negative_volume(80_773.616, -0.0005)]
    fn unsupported_streamed_values_drop_the_side(#[case] price: f64, #[case] cum_volume: f64) {
        let book = synthetic_book(
            vec![level(price, cum_volume)],
            vec![level(80801.55138978669, 0.00012376)],
        );

        let (sell_wbtc, sell_usdc) = ladders_for_book(&book, &wbtc(), &usdc());

        assert_eq!(sell_wbtc, Err(SideRejection::UnsupportedLevel));
        assert_eq!(sell_usdc, Ok(vec![quote(10_000_000, 12_376)]));
    }

    /// Synthetic: an amount that overflows `f64` has no known size.
    #[test]
    fn non_finite_amounts_drop_the_side() {
        let book = synthetic_book(vec![level(f64::MAX, 1.0)], Vec::new());

        let (sell_wbtc, _) = ladders_for_book(&book, &wbtc(), &usdc());

        assert_eq!(sell_wbtc, Err(SideRejection::NonFiniteAmount));
    }

    /// Synthetic: a knot at a zero amount is not a quote, and the ladder interpolates across its
    /// knots, so keeping the rest of the side would invent depth. The interior case is the
    /// zero-decimals counterexample from the design: dropping its middle knot instead would
    /// interpolate a fabricated output of 200 at an input of 2.
    #[rstest]
    #[case::boundary(vec![level(0.004, 0.000_000_01), level(80_000.0, 0.001)], 8, 6)]
    #[case::interior(vec![level(100.0, 1.0), level(0.05, 2.0), level(33.333, 3.0)], 0, 0)]
    fn zero_quantized_amounts_drop_the_side(
        #[case] bids: Vec<TitanLevel>,
        #[case] decimals1: u32,
        #[case] decimals2: u32,
    ) {
        let book = synthetic_book(bids, Vec::new());
        let token1 = token(WBTC, "WBTC", decimals1);
        let token2 = token(USDC, "USDC", decimals2);

        let (sell_token1, _) = ladders_for_book(&book, &token1, &token2);

        assert_eq!(sell_token1, Err(SideRejection::ZeroAmount));
    }

    /// Synthetic, with zero-decimals tokens so both volumes round to one base unit: a side that
    /// quotes two outputs for one input is not a price function.
    #[test]
    fn conflicting_knots_drop_the_side() {
        let book = synthetic_book(vec![level(200.0, 0.6), level(100.0, 1.4)], Vec::new());
        let token1 = token(WBTC, "WBTC", 0);
        let token2 = token(USDC, "USDC", 0);

        let (sell_token1, _) = ladders_for_book(&book, &token1, &token2);

        assert_eq!(sell_token1, Err(SideRejection::ConflictingKnots));
    }

    /// Synthetic: Titan repeats a cumulative volume across consecutive levels, which is no
    /// conflict as long as both levels price it the same.
    #[test]
    fn repeated_knots_that_agree_are_kept() {
        let book =
            synthetic_book(vec![level(80_773.616, 0.0005), level(80_773.616, 0.0005)], Vec::new());

        let (sell_wbtc, _) = ladders_for_book(&book, &wbtc(), &usdc());

        assert_eq!(sell_wbtc, Ok(vec![quote(50_000, 40_386_808), quote(50_000, 40_386_808)]));
    }
}
