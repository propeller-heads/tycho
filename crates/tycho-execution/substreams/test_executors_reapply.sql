-- Assertions for a second run of executors.sql, after a hand-inserted executor and a hand-edited
-- row that the file also carries.
DO $$
BEGIN
    -- A row the file does not carry survives.
    IF (SELECT protocol_systems FROM executors
        WHERE chain = 'ethereum' AND address = '0xdeadbeef00000000000000000000000000000000')
       IS DISTINCT FROM ARRAY['brand_new_amm'] THEN
        RAISE EXCEPTION 'a hand-inserted executor did not survive the re-run: %',
            (SELECT protocol_systems FROM executors
             WHERE chain = 'ethereum' AND address = '0xdeadbeef00000000000000000000000000000000');
    END IF;

    -- A row the file does carry keeps what the database holds. The file is the list of executors
    -- to add, not a definition of the rows that exist, so applying it never overwrites one.
    IF (SELECT protocol_systems FROM executors
        WHERE chain = 'ethereum' AND address = '0xfee95e97db5fdfcde672b9a06f4be87032dd7689')
       IS DISTINCT FROM ARRAY['edited_by_hand'] THEN
        RAISE EXCEPTION 'the re-run overwrote a row the database already had: %',
            (SELECT protocol_systems FROM executors
             WHERE chain = 'ethereum' AND address = '0xfee95e97db5fdfcde672b9a06f4be87032dd7689');
    END IF;

    -- The hand-inserted executor now names the hop that was unknown before.
    IF (SELECT protocol_systems FROM trade_protocol_systems WHERE trade_id = 't3')
       IS DISTINCT FROM ARRAY['brand_new_amm'] THEN
        RAISE EXCEPTION 'the new executor did not name the stored trade: %',
            (SELECT protocol_systems FROM trade_protocol_systems WHERE trade_id = 't3');
    END IF;
END $$;
