DO $$
DECLARE
    v RECORD;
BEGIN
    -- Each chain anchors on its own stable: ethereum 2000 USD, base 1000 USD.
    IF (SELECT native_usd FROM trades WHERE id = 'in-preferred') IS DISTINCT FROM 2000.0 THEN
        RAISE EXCEPTION 'ethereum anchor wrong: %',
            (SELECT native_usd FROM trades WHERE id = 'in-preferred');
    END IF;
    IF (SELECT native_usd FROM trades WHERE id = 'base-preferred') IS DISTINCT FROM 1000.0 THEN
        RAISE EXCEPTION 'base anchor wrong: %',
            (SELECT native_usd FROM trades WHERE id = 'base-preferred');
    END IF;

    -- 1 token at 2 native x 2000 USD.
    SELECT * INTO v FROM trades WHERE id = 'in-preferred';
    IF v.volume_usd IS DISTINCT FROM 4000.0 OR v.price_in_usd IS DISTINCT FROM 4000.0
       OR v.price_source IS DISTINCT FROM 'in_preferred' THEN
        RAISE EXCEPTION 'preferred in-side pricing wrong: % % %',
            v.volume_usd, v.price_in_usd, v.price_source;
    END IF;
    -- The out token is unknown, so it gets no unit price and no decimals.
    IF v.price_out_usd IS NOT NULL OR v.decimals_out IS NOT NULL THEN
        RAISE EXCEPTION 'unknown out token was given a price';
    END IF;

    -- A stable prices a 30-day-old trade: 1500 units at 1 USD.
    SELECT * INTO v FROM trades WHERE id = 'out-stable-old';
    IF v.volume_usd IS DISTINCT FROM 1500.0 OR v.price_source IS DISTINCT FROM 'out_stable' THEN
        RAISE EXCEPTION 'stable pricing of an old trade wrong: % %',
            v.volume_usd, v.price_source;
    END IF;
    -- The trade implies the price of the token on the other side: 1500 USD for 1 token.
    IF v.price_in_usd IS NOT NULL THEN
        RAISE EXCEPTION 'in token has no decimals, so it must have no unit price';
    END IF;

    -- Both sides preferred: priority 1 (the stable) wins over priority 2.
    SELECT * INTO v FROM trades WHERE id = 'both-preferred';
    IF v.volume_usd IS DISTINCT FROM 3000.0 OR v.price_source IS DISTINCT FROM 'out_stable' THEN
        RAISE EXCEPTION 'priority between two preferred sides wrong: % %',
            v.volume_usd, v.price_source;
    END IF;
    -- Tycho prices the in token at 4000, but it did not value the trade, so it takes the price
    -- this trade implies: 3000 USD for the 1 token that went in.
    IF v.price_in_usd IS DISTINCT FROM 3000.0 THEN
        RAISE EXCEPTION 'the side that did not value the trade must take the implied price, got %',
            v.price_in_usd;
    END IF;
    IF v.price_out_usd IS DISTINCT FROM 1.0 THEN
        RAISE EXCEPTION 'the valued side must keep its own unit price, got %', v.price_out_usd;
    END IF;

    -- The native sentinel is one native token.
    SELECT * INTO v FROM trades WHERE id = 'native-in';
    IF v.volume_usd IS DISTINCT FROM 2000.0 OR v.price_source IS DISTINCT FROM 'in_preferred' THEN
        RAISE EXCEPTION 'native sentinel pricing wrong: % %', v.volume_usd, v.price_source;
    END IF;

    -- Tycho prices the out side of a fresh trade with no preferred token: 2 units at 2000 USD.
    SELECT * INTO v FROM trades WHERE id = 'out-tycho';
    IF v.volume_usd IS DISTINCT FROM 4000.0 OR v.price_source IS DISTINCT FROM 'out_tycho' THEN
        RAISE EXCEPTION 'tycho fallback wrong: % %', v.volume_usd, v.price_source;
    END IF;

    -- base: 4 units of a 500 USD token.
    SELECT * INTO v FROM trades WHERE id = 'base-preferred';
    IF v.volume_usd IS DISTINCT FROM 2000.0 OR v.price_source IS DISTINCT FROM 'out_preferred' THEN
        RAISE EXCEPTION 'base preferred pricing wrong: % %', v.volume_usd, v.price_source;
    END IF;

    -- Every priced row must be internally consistent: the unit price of a side times its amount
    -- is the volume. This is what an implied price on the non-valued side buys, and it breaks as
    -- soon as a current Tycho price is stamped on one side of an old trade.
    IF EXISTS (
        SELECT 1 FROM trades
        WHERE volume_usd > 0 AND amount_in > 0 AND amount_out > 0
          AND decimals_in IS NOT NULL AND decimals_out IS NOT NULL
          AND (abs(price_in_usd * (amount_in / power(10::numeric, decimals_in)) - volume_usd)
                   / volume_usd > 0.0001
            OR abs(price_out_usd * (amount_out / power(10::numeric, decimals_out)) - volume_usd)
                   / volume_usd > 0.0001)
    ) THEN
        RAISE EXCEPTION 'a priced trade is not internally consistent: %', (
            SELECT string_agg(id, ', ') FROM trades
            WHERE volume_usd > 0 AND amount_in > 0 AND amount_out > 0
              AND decimals_in IS NOT NULL AND decimals_out IS NOT NULL
              AND (abs(price_in_usd * (amount_in / power(10::numeric, decimals_in)) - volume_usd)
                       / volume_usd > 0.0001
                OR abs(price_out_usd * (amount_out / power(10::numeric, decimals_out)) - volume_usd)
                       / volume_usd > 0.0001)
        );
    END IF;

    IF EXISTS (
        SELECT 1 FROM trades
        WHERE id IN ('old-nonstable', 'failed-call', 'stale-price', 'out-of-band')
          AND priced_at IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'an ineligible trade was priced: %', (
            SELECT string_agg(id, ', ') FROM trades
            WHERE id IN ('old-nonstable', 'failed-call', 'stale-price', 'out-of-band')
              AND priced_at IS NOT NULL
        );
    END IF;
END $$;
