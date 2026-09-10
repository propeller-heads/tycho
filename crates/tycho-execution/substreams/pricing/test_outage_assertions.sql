DO $$
BEGIN
    IF (SELECT volume_usd FROM trades WHERE id = 'outage-probe') IS DISTINCT FROM 4000.0 THEN
        RAISE EXCEPTION 'healthy-chain pricing stopped during another chain outage';
    END IF;
    IF (SELECT priced_at FROM trades WHERE id = 'base-preferred') IS NULL THEN
        RAISE EXCEPTION 'base fixture was not priced before the outage';
    END IF;
END $$;
