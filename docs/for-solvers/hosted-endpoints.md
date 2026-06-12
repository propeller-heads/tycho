---
description: Tycho Indexer's hosted endpoints
---

# Hosted Endpoints

## Tycho Indexer

<table><thead><tr><th width="244">Chain</th><th>URL</th></tr></thead><tbody><tr><td>Ethereum (Mainnet)</td><td>tycho-beta.propellerheads.xyz</td></tr><tr><td>Base Mainnet</td><td>tycho-base-beta.propellerheads.xyz</td></tr><tr><td>Unichain Mainnet</td><td>tycho-unichain-beta.propellerheads.xyz</td></tr><tr><td>Arbitrum Mainnet</td><td>tycho-arbitrum-beta.propellerheads.xyz</td></tr><tr><td>Polygon Mainnet</td><td>tycho-polygon-beta.propellerheads.xyz</td></tr></tbody></table>

### Tycho Fynd

Tycho Fynd endpoints are dedicated instances for Fynd users. They enforce stricter data filtering restrictions to support higher load.

<table><thead><tr><th width="244">Chain</th><th>URL</th></tr></thead><tbody><tr><td>Ethereum (Mainnet)</td><td>tycho-fynd-ethereum.propellerheads.xyz</td></tr><tr><td>Base Mainnet</td><td>tycho-fynd-base.propellerheads.xyz</td></tr><tr><td>Unichain Mainnet</td><td>tycho-fynd-unichain.propellerheads.xyz</td></tr><tr><td>Arbitrum Mainnet</td><td>tycho-fynd-arbitrum.propellerheads.xyz</td></tr><tr><td>Polygon Mainnet</td><td>tycho-fynd-polygon.propellerheads.xyz</td></tr></tbody></table>

{% hint style="info" %}
For API Documentation, Tycho Indexer includes Swagger docs, available at /docs/ path.

Example, for Mainnet: <a href="https://tycho-beta.propellerheads.xyz/docs/" target="_blank" rel="noopener noreferrer">https://tycho-beta.propellerheads.xyz/docs/</a>
{% endhint %}

{% hint style="info" %}
We're constantly adding new protocols. To see the current protocol systems for a specific endpoint, use the [Retrieve protocol systems](indexer/tycho-rpc.md#v1-protocol_systems) endpoint.
{% endhint %}

## Plans

Each API key is assigned a plan that determines rate limits and endpoint access.

### Rate Limits

<table><thead><tr><th>Plan</th><th>Requests/sec</th><th>Burst</th><th>Max WebSocket Connections</th><th>Allowed Endpoints</th></tr></thead><tbody><tr><td>basic</td><td>50</td><td>300/s (6x)</td><td>2</td><td>Tycho Indexer, Tycho Fynd</td></tr><tr><td>fynd-basic</td><td>50</td><td>300/s (6x)</td><td>2</td><td>Tycho Fynd only</td></tr></tbody></table>

{% hint style="info" %}
Don't have a key yet? Get a base-tier key from [@fynd_portal_bot](https://t.me/fynd_portal_bot) on Telegram.

Need higher limits or full Tycho Indexer access? DM `@tanay_j` on Telegram to request an upgrade.
{% endhint %}

### Data Restrictions

Endpoints enforce data filtering restrictions on API queries. When a restriction is active, requests that do not include the required parameter values are rejected.

#### Tycho Indexer

<table><thead><tr><th width="280">Restriction</th><th>Value</th></tr></thead><tbody><tr><td>Max version age</td><td>10 minutes</td></tr><tr><td>Protocol systems</td><td>All available</td></tr><tr><td><code>tvl_gt</code></td><td>No restriction</td></tr><tr><td><code>min_quality</code></td><td>No restriction</td></tr><tr><td><code>traded_n_days_ago</code></td><td>No restriction</td></tr></tbody></table>

#### Tycho Fynd

All Fynd endpoints share the same filtering restrictions:

<table><thead><tr><th width="280">Restriction</th><th>Value</th></tr></thead><tbody><tr><td>Max version age</td><td>10 minutes</td></tr><tr><td><code>tvl_gt</code></td><td>Chain-dependent. See <a href="indexer/tycho-client/README.md#tvl-filtering">TVL Filtering</a> for values per chain.</td></tr><tr><td><code>min_quality</code></td><td>100</td></tr><tr><td><code>traded_n_days_ago</code></td><td>3</td></tr></tbody></table>

The available protocol systems vary by chain:

<table><thead><tr><th width="244">Chain</th><th>Protocol Systems</th></tr></thead><tbody><tr><td>Ethereum (Mainnet)</td><td><code>uniswap_v2</code>, <code>uniswap_v3</code>, <code>uniswap_v4</code>, <code>sushiswap_v2</code>, <code>pancakeswap_v2</code>, <code>pancakeswap_v3</code>, <code>ekubo_v2</code>, <code>ekubo_v3</code>, <code>fluid_v1</code></td></tr><tr><td>Base Mainnet</td><td><code>uniswap_v2</code>, <code>uniswap_v3</code>, <code>uniswap_v4</code>, <code>pancakeswap_v3</code>, <code>aerodrome_slipstreams</code></td></tr><tr><td>Unichain Mainnet</td><td><code>uniswap_v2</code>, <code>uniswap_v3</code>, <code>uniswap_v4</code>, <code>velodrome_slipstreams</code>, <code>uniswap_v4_hooks</code></td></tr><tr><td>Arbitrum Mainnet</td><td><code>uniswap_v2</code>, <code>uniswap_v3</code>, <code>uniswap_v4</code>, <code>pancakeswap_v3</code></td></tr><tr><td>Polygon Mainnet</td><td><code>uniswap_v2</code>, <code>uniswap_v3</code>, <code>uniswap_v4</code>, <code>quickswap_v2</code></td></tr></tbody></table>

## Metrics

{% embed url="http://tycho.live" %}
