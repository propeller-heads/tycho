DO $$
BEGIN
    IF (SELECT volume_eth FROM trades WHERE id = 'outage-probe') <> 2.0 THEN
        RAISE EXCEPTION 'healthy-chain pricing stopped during another chain outage';
    END IF;
    IF (SELECT priced_at FROM trades WHERE id = 'base-output') IS NULL THEN
        RAISE EXCEPTION 'base fixture was not priced before the outage';
    END IF;
END $$;
