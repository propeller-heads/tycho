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
    -- The in token is preferred and keeps its own price rather than the implied one.
    IF v.price_in_usd IS DISTINCT FROM 4000.0 THEN
        RAISE EXCEPTION 'preferred in token lost its own unit price: %', v.price_in_usd;
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
