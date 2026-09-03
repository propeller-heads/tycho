DO $$
BEGIN
    IF (SELECT volume_eth FROM trades WHERE id = 'ethereum-input') <> 2.0 THEN
        RAISE EXCEPTION 'input-token pricing failed';
    END IF;
    IF (SELECT volume_eth FROM trades WHERE id = 'base-output') <> 1.0 THEN
        RAISE EXCEPTION 'output-token fallback failed';
    END IF;
    IF (SELECT volume_eth FROM trades WHERE id = 'ethereum-native') <> 1.0 THEN
        RAISE EXCEPTION 'native ETH pricing failed';
    END IF;
    IF EXISTS (
        SELECT 1 FROM trades
        WHERE id IN ('failed-call', 'old-trade', 'stale-price')
          AND priced_at IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'an ineligible trade was priced';
    END IF;
END $$;
