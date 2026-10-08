-- Executor address to protocol names, keyed by chain.
--
-- The package writes the executor address of every hop, so a name is resolved at query time.
-- That makes a newly deployed executor one INSERT instead of a new wasm build, a new module hash
-- and a cursor reset, and it names the executor on every trade already stored.
--
-- ADD A NEW EXECUTOR HERE. This file is the list, kept by hand; append a row in chain order and
-- commit it. The rows it starts with were read off the git history of
-- docs/for-solvers/execution/contract-addresses.md and
-- crates/tycho-execution/config/executor_addresses.json.
--
-- Every sink and the pricer apply the file on start, and the INSERT does nothing to a row that
-- is already there. So a row added straight to the database survives, and CHANGING A ROW HERE
-- DOES NOT CHANGE THE DATABASE: to correct one, delete it and apply the file again.
--
--   psql "$DSN" -f executors.sql
BEGIN;

-- Every sink and the pricer apply this file on start. The lock makes them wait for each other
-- instead of racing on the same DDL.
DO $$ BEGIN PERFORM pg_advisory_xact_lock(6045170023); END $$;

CREATE TABLE IF NOT EXISTS executors (
    chain            TEXT   NOT NULL,
    address          TEXT   NOT NULL,
    protocol_systems TEXT[] NOT NULL,
    PRIMARY KEY (chain, address)
);

INSERT INTO executors (chain, address, protocol_systems) VALUES
    ('arbitrum', '0x117aba0cc2fc158318cc5640a3f8da0c426cd4ab', ARRAY['native_wrapper']),
    ('arbitrum', '0xcaaac0c6193e3e2e3e8e94baf6367f75bae591c9', ARRAY['pancakeswap_v3', 'uniswap_v3']),
    ('arbitrum', '0xce5af637bffe0c34a37f34471ab6a3e90adf8ffb', ARRAY['rfq:metric']),
    ('arbitrum', '0xd0a1f74d4d77834feca0efa01ca6e0161dbf1f57', ARRAY['uniswap_v2']),
    ('arbitrum', '0xdb696336f7a5f9048252664a3475c194dae0e62f', ARRAY['uniswap_v4']),
    ('base', '0x21a97f2aa8d9ae7144bfd080266ee87cc2369656', ARRAY['pancakeswap_v3']),
    ('base', '0x321c00604355dd20a2cb472d44fe8f37d821a82c', ARRAY['sushiswap_v2', 'uniswap_v2']),
    ('base', '0x489a3f531da3873d6585bf3f8e0dee48cac6f7bc', ARRAY['bebop']),
    ('base', '0x647bffbf8bd72bf6341ecba8b0279e090313a40d', ARRAY['uniswap_v3']),
    ('base', '0x68af9cdcc2e177e9d75c60f132b8dc52eaf1adbf', ARRAY['native_wrapper']),
    ('base', '0x6e644b877c1247c8ab8613fe0787a7b444768d23', ARRAY['pancakeswap_v3', 'uniswap_v3']),
    ('base', '0x711ea5d03541ab0ee84711d1ed92b5de0bb178c1', ARRAY['uniswap_v4']),
    ('base', '0x76eeb7e737fdf441e0437f7051b4e0b84808c10a', ARRAY['aerodrome_v1']),
    ('base', '0x772f58a21a1e64c5677cdf16630205264cad2b6a', ARRAY['slipstreams']),
    ('base', '0x78db9684220541601e9215bb16b219e5df6cf0fb', ARRAY['uniswap_v4']),
    ('base', '0x8a3520889fe0bbf9e1f4a9724c27d8d6ed9f0e29', ARRAY['uniswap_v4']),
    ('base', '0x8ae978713c6952041d058d8a3fe3a2568f1e1ae1', ARRAY['native_wrapper', 'weth']),
    ('base', '0x8fccdb466b71b715355919b3db0f6b14bfba9dd7', ARRAY['aerodrome_slipstreams']),
    ('base', '0xa2f2d75f08cdf36add125da906d42737527e4e30', ARRAY['weth']),
    ('base', '0xb8644a264a4b765e60c4a65fb8049eae1a791d97', ARRAY['rfq:metric']),
    ('base', '0xbf9b925b6f5f6a5919f70e62d56403948f27f882', ARRAY['pancakeswap_v3', 'uniswap_v3']),
    ('base', '0xbfb5fbfd4c4182d3a1df91ced45335c543bed3f0', ARRAY['rfq:bebop']),
    ('base', '0xcdf4e28676b9f2e8c6a722eb163e0c2f9c0716b7', ARRAY['rfq:bebop']),
    ('base', '0xd689b184c250e543eb3938d524733ff6b4cfc296', ARRAY['uniswap_v2']),
    ('base', '0xe12eb9fcd93c7ffabfed66b20d4b0df02b44f6b3', ARRAY['aerodrome_slipstreams']),
    ('base', '0xf35839c54a33fe88867acb3deac9db3adb9bfa78', ARRAY['rfq:bebop']),
    ('base', '0xf435299a20c1405c957c3f407cc3e9b9df76ee7b', ARRAY['lunar_base', 'lunarbase']),
    ('base', '0xf744ebfaa580cf3ffc25ad046e92bd8b770a0700', ARRAY['uniswap_v2']),
    ('bsc', '0x067fbaf88ee89dc2a4368c6dff7237ff84522577', ARRAY['pancake_v3', 'pancakeswap_v3', 'uniswap_v3']),
    ('bsc', '0x0ecbe4a09eff3f4d054723720f60b55ee52bd99e', ARRAY['pancake_v2', 'pancakeswap_v2']),
    ('bsc', '0x450b4a643553912c9af98dc8b9db2ef989fb1444', ARRAY['uniswap_v2']),
    ('bsc', '0x8a61866f7f9c979bee4426247e144add46cd21aa', ARRAY['rfq:metric']),
    ('bsc', '0x8c41b3842d767bb07818e9e82fb62964910a82ef', ARRAY['native_wrapper']),
    ('bsc', '0x925a1ae9494ec8d92c2900dcdd6ca9ee621e10ed', ARRAY['uniswap_v4']),
    ('ethereum', '0x0017c84f2b3414514b67bfc9a63830c8e0e690d0', ARRAY['sushiswap_v2', 'uniswap_v2']),
    ('ethereum', '0x08ba9e85c8944098deaace0e164b7ec379a7e810', ARRAY['ring_swap_v2']),
    ('ethereum', '0x11c1951e404e1a2a18a046f22501019a02c23d0b', ARRAY['erc4626']),
    ('ethereum', '0x128ba676f1426d0260f7f1eedd799777dbe12fdb', ARRAY['ekubo_v3']),
    ('ethereum', '0x19e49db786c87f4e46b10afb21c0c06d34270f98', ARRAY['hashflow']),
    ('ethereum', '0x1f6ecc09b7753b1f637ed702ade44b5ba034dd5a', ARRAY['rfq:bebop']),
    ('ethereum', '0x1f8b52899dcf1be69eafc754ca3e099ff15f03ae', ARRAY['liquidity_party', 'vm:liquidityparty']),
    ('ethereum', '0x2017ad7035d781c14699c8e44ed62d3083723a18', ARRAY['sushiswap_v2']),
    ('ethereum', '0x25b670a94a376254bac2b5f16b0dc040df44d1ec', ARRAY['ekubo_v2']),
    ('ethereum', '0x2605605cbbcfec01d15704db1a47a0ac48d1f729', ARRAY['rocketpool']),
    ('ethereum', '0x263dd7ad20983b5e0392bf1f09c4493500edb333', ARRAY['ekubo']),
    ('ethereum', '0x3201ea6b93731f30e55cb87660eed70b369fadc7', ARRAY['pancakeswap_v2']),
    ('ethereum', '0x38e6dca40c5c96ca4f5e3f7583571e723f11836f', ARRAY['vm:balancer_v2']),
    ('ethereum', '0x3bd39a3606b9dc0afd07ea1c865c4583d94cba60', ARRAY['rfq:liquorice']),
    ('ethereum', '0x63a1fff3a4b6e93d5687f7b340ff51ba3a88901c', ARRAY['bop_amm', 'vm:bopamm']),
    ('ethereum', '0x667cb014f2b3c470b53089d984c3cca840d23052', ARRAY['pancakeswap_v3', 'uniswap_v3']),
    ('ethereum', '0x67dd87c719d0ea8499346693978cbee1a743eec9', ARRAY['native_wrapper', 'weth']),
    ('ethereum', '0x69eae6eb2c924567912fbc435b915663c9d3d11e', ARRAY['rfq:bebop']),
    ('ethereum', '0x6ad86dec4c9b897640730eaedf8ff4659a3be8be', ARRAY['rocketpool']),
    ('ethereum', '0x7191a6a3006ea020c73acd7068295b9b3767a3bb', ARRAY['fluid_v1']),
    ('ethereum', '0x79087adb525a6d4e20799ae68ac06de0a15c278e', ARRAY['sushiswap_v2', 'uniswap_v2']),
    ('ethereum', '0x81c7febeed3fd67a5fdcf892d43489091caeb186', ARRAY['rfq:liquorice']),
    ('ethereum', '0x8594ac3486b6c68df5bf5f9add25fdcac69f2588', ARRAY['vm:balancer_v3']),
    ('ethereum', '0x8b2042d7be0be18303464362d39d667ca5e74d42', ARRAY['weth']),
    ('ethereum', '0x8fd892dd5f8e153fe15cc3a7567e18ed6f7b1e0a', ARRAY['native_wrapper']),
    ('ethereum', '0x93fc40cd88b54f2cbcbf182fa1c78522805b213a', ARRAY['rfq:hashflow']),
    ('ethereum', '0x95ca663a10736a748981139f3071cdb21baac954', ARRAY['liquidity_party', 'vm:liquidityparty']),
    ('ethereum', '0x95e8d6e3997d98170ab7243dfecf93b5f5e25bed', ARRAY['vm:maverick_v2']),
    ('ethereum', '0x9d32e9f569b22ae8d8c6f788037c1cd53632a059', ARRAY['pancakeswap_v3']),
    ('ethereum', '0x9d517d5a3a3266fbd75b1ad4fe6cfc40087cfdc0', ARRAY['vm:balancer_v2']),
    ('ethereum', '0xa13138a3ae9311b345405512ac4040ccdeebf258', ARRAY['ekubo_v3']),
    ('ethereum', '0xa2f9f6f4a3f64eda6014ce71a8ec8a874d690adf', ARRAY['prop_amm_fallback', 'propammfallback']),
    ('ethereum', '0xa942c54f2e58153ecdd4dd24b9bf98f57c9d7d55', ARRAY['uniswap_v4']),
    ('ethereum', '0xab081cbb3c88219a030928ece277fead99cab742', ARRAY['fluid_v1']),
    ('ethereum', '0xac51c531555b7154fe1bcb7cf4508e93a3beda75', ARRAY['rfq:liquorice']),
    ('ethereum', '0xae04ca7e9ed79cbd988f6c536ce11c621166f41b', ARRAY['uniswap_v2']),
    ('ethereum', '0xae3ceee21cb9be4c6c70de0912947b537bc00b61', ARRAY['fluid_v1']),
    ('ethereum', '0xaf0e1ac9ea1a81120bf4f285340ac70e41c9d65f', ARRAY['vm:curve']),
    ('ethereum', '0xb5b8dc3f0a1be99685a0ded015af93bfbb55c411', ARRAY['balancer_v2']),
    ('ethereum', '0xbab7124c9662b15c6b9af0b1f329907dd55a24fc', ARRAY['uniswap_v3']),
    ('ethereum', '0xbc7c1e078865bb81938adddf24582eedd17324c7', ARRAY['rfq:hashflow']),
    ('ethereum', '0xc196f22d69d41a2b1dbf6ef8d28c09bc2ca4e4fa', ARRAY['pricelevelstream', 'prop_amm']),
    ('ethereum', '0xc77a1a9b8ebecc6bfb386720e0a37bc900860f5b', ARRAY['ekubo_v3']),
    ('ethereum', '0xc7d47f3c3f755ed977f3c19f4c1f007cbed109b0', ARRAY['pancakeswap_v3', 'uniswap_v3']),
    ('ethereum', '0xc8031d1457d19d5f0e074f74960baf2010bea795', ARRAY['curve']),
    ('ethereum', '0xc9db3feb380e4fd9af239e2595ecdece3b5c34a4', ARRAY['pancakeswap_v2']),
    ('ethereum', '0xd2798b649e965e5d33fd7d36c87b1fd6709d0b04', ARRAY['uniswap_v4']),
    ('ethereum', '0xd58b803dbe049903f3c1adaacc2d82e9e514647c', ARRAY['ekubo_v3']),
    ('ethereum', '0xd74644f4ed013dc5f63fe2a576e5fbf6070aec00', ARRAY['rfq:bebop']),
    ('ethereum', '0xd95d619fa709ba559d9cee7645c5f71dbcf19f0f', ARRAY['rfq:metric']),
    ('ethereum', '0xda551e2aa856101ea172f0f6efe25bf27dfdf9e0', ARRAY['rocketpool']),
    ('ethereum', '0xe2c352232127afc9803e7ed79f129e458b20925e', ARRAY['fermi_swap', 'vm:fermiswap']),
    ('ethereum', '0xe49b916032c734cd89cdfe80a868805c738a6ceb', ARRAY['uniswap_v4']),
    ('ethereum', '0xe7b267d06df83c8fecad18af8be0cef54068f138', ARRAY['fluid_v1']),
    ('ethereum', '0xec5ce4bf6fbcb7bb0148652c92a4aec8c1d474ec', ARRAY['balancer_v3']),
    ('ethereum', '0xecd7d651c0d5e477018c0949a01a46350461c8ed', ARRAY['vm:maverick_v2']),
    ('ethereum', '0xee2452ce461ea71e15f06af8055ffaad4b7b90ae', ARRAY['erc4626']),
    ('ethereum', '0xf35e3f5f205769b41508a18787b62a21bc80200b', ARRAY['maverick']),
    ('ethereum', '0xf7561795536ede0f4122bed2cc9328511e1f7135', ARRAY['vm:curve']),
    ('ethereum', '0xf9ae8127e35612da9d750058fcf61f326e00ca10', ARRAY['vm:balancer_v3']),
    ('ethereum', '0xfb89ca30e5ff1863237dd24aaf2189bd1ca6cc0d', ARRAY['pancakeswap_v2']),
    ('ethereum', '0xfe42bfb115ed9671011ca52bdd23a52a2e077a7c', ARRAY['bebop']),
    ('ethereum', '0xfee95e97db5fdfcde672b9a06f4be87032dd7689', ARRAY['ekubo_v2']),
    ('plasma', '0x1d9220ea65ca79997c532ba8fbc79b1c463bd505', ARRAY['native_wrapper']),
    ('plasma', '0x5020a5012cdc3545d2e98b6fb3ea4b9dc90552a6', ARRAY['fluid_v1']),
    ('plasma', '0xa6d43efec7870ac6e27c81232c2b5e11b17d40d4', ARRAY['uniswap_v3']),
    ('plasma', '0xc015bc2f482b6cf31d6c30e663a57417a6761949', ARRAY['vm:curve']),
    ('polygon', '0x32bb33afb193e16df121cdc2d87c44f015d325da', ARRAY['uniswap_v4']),
    ('polygon', '0x4c409a96de96e06f061ce55b15cd318d3dd2239c', ARRAY['ramses_v3', 'uniswap_v3']),
    ('polygon', '0x746cdaf8ef08a9d023155dddc95291ab8c9a7936', ARRAY['rfq:metric']),
    ('polygon', '0xb9f140f4f8d674c453b3ee00646189452a74e10c', ARRAY['quickswap_v2', 'uniswap_v2']),
    ('polygon', '0xc8f2a896d6d98b325889ce9c998f62779d17fee3', ARRAY['native_wrapper']),
    ('robinhood', '0x33e8602fedd1215cb04c865c66648dba5b81fb75', ARRAY['ramses_v3', 'robinswap_v3', 'sushiswap_v3', 'uniswap_v3']),
    ('robinhood', '0x6ead915f443eb0f8758be58afe7ef2df1e2b927f', ARRAY['native_wrapper']),
    ('robinhood', '0x81afb56e21ab0744330acf400e2a65e6b64d495e', ARRAY['uniswap_v2']),
    ('robinhood', '0xe781c1869c9d8e60ddfcd8f8fb5213ed8ad07366', ARRAY['uniswap_v4']),
    ('unichain', '0x00c1b81e3c8f6347e69e2ddb90454798a6be975e', ARRAY['uniswap_v2']),
    ('unichain', '0x052a40ab2875437e0ff7f68976954ad62ab1f235', ARRAY['uniswap_v4']),
    ('unichain', '0x0847ec6467499a6f31fdd06b7deb766649b2739b', ARRAY['uniswap_v3']),
    ('unichain', '0x13acf9532c173753484ed1bef86c98cbfe83ba90', ARRAY['weth']),
    ('unichain', '0x32ed490fb53cc66c3cbddc74ab8b7088f11d6289', ARRAY['native_wrapper']),
    ('unichain', '0x3ae67e7dcfa6c5ab36da9de3b67b7496956e0874', ARRAY['uniswap_v4']),
    ('unichain', '0x45356073646354f162982bed83c71ef0f310e201', ARRAY['uniswap_v4']),
    ('unichain', '0x50905e0f94350f9e533cd275667afc728265960d', ARRAY['native_wrapper', 'weth']),
    ('unichain', '0x55345739fcb5c5393f9be32864439754649a204a', ARRAY['uniswap_v2']),
    ('unichain', '0x6794ce07fddc3bb5f747b4510d0ebb0d11a03f2c', ARRAY['uniswap_v2']),
    ('unichain', '0x685733edb7654e540ab537eabc28b554646b69d0', ARRAY['vm:curve']),
    ('unichain', '0x7270991eb529c7e45a3ec98de542b01e4da32d82', ARRAY['uniswap_v3']),
    ('unichain', '0xb208092276fde05cff20341049f1e384b1b31112', ARRAY['velodrome_slipstreams']),
    ('unichain', '0xbc4d9e944ad40480a34ebaf38cd2acf6e1dc0def', ARRAY['vm:curve']),
    ('unichain', '0xd26a838a41af3d4815dfd745a080b2062c4124d1', ARRAY['uniswap_v3']),
    ('unichain', '0xd9ee40c6895de968b41b96253970b48fe852e5ef', ARRAY['velodrome_slipstreams'])
ON CONFLICT (chain, address) DO NOTHING;

DO $$
BEGIN
    IF to_regclass('public.trade_hops') IS NULL THEN
        RAISE NOTICE 'trade tables do not exist yet, skipping the views';
        RETURN;
    END IF;

    CREATE INDEX IF NOT EXISTS trade_hops_executor_idx ON trade_hops (chain, executor);

    -- One row per hop, with the protocol names of its executor. Empty for an executor this
    -- table does not know.
    --
    -- The row of the hop's own chain wins. An executor is deployed at the same address on
    -- several chains and the docs list it under one of them, so an address with no row for
    -- this chain falls back to the names it carries on the chains that do have one.
    CREATE OR REPLACE VIEW trade_hop_protocols AS
    SELECT h.trade_id,
           h.chain,
           h.hop_index,
           h.executor,
           COALESCE(e.protocol_systems, ARRAY[]::TEXT[]) AS protocol_systems
    FROM trade_hops h
    LEFT JOIN LATERAL (
        SELECT COALESCE(
            (SELECT x.protocol_systems FROM executors x
              WHERE x.chain = h.chain AND x.address = h.executor),
            (SELECT array_agg(DISTINCT s.name ORDER BY s.name)
               FROM executors x CROSS JOIN LATERAL unnest(x.protocol_systems) AS s(name)
              WHERE x.address = h.executor)
        ) AS protocol_systems
    ) e ON TRUE;

    -- One row per trade, with the distinct protocol names of all its hops. A trade whose
    -- executors are all unknown has no row here.
    CREATE OR REPLACE VIEW trade_protocol_systems AS
    SELECT p.trade_id,
           p.chain,
           array_agg(DISTINCT s.name ORDER BY s.name) AS protocol_systems
    FROM trade_hop_protocols p
    CROSS JOIN LATERAL unnest(p.protocol_systems) AS s(name)
    GROUP BY p.trade_id, p.chain;
END $$;

COMMIT;
