# Tycho RPC

Tycho exposes data through two mechanisms, the RPC and the stream. The RPC provides you access to static data, like the state of a component at a given block or extended information about the tokens it has found. For streaming data, we recommend using the [tycho-client](tycho-client/ "mention"). This guide documents the RPC interfaces.&#x20;

### Token Information

Tycho stream provides only the token addresses that Protocol Components use. If you require more token information, you can request using [tycho-rpc.md](tycho-rpc.md "mention")'s [#v1-tokens](tycho-rpc.md#v1-tokens "mention")endpoint. This service allows filtering by both quality and activity.

#### Quality Token Quality Ratings

The quality rating system helps you quickly assess token's specific properties:

* **100**: Normal ERC-20 Token behavior
* **75**: Rebasing token
* **50**: Fee-on-transfer token
* **10**: Token analysis failed at first detection
* **5**: Token analysis failed multiple times (after creation)
* **0**: Failed to extract attributes, like Decimal or Symbol

{% hint style="info" %}
The Token Quality Analysis was developed to aid Tycho Simulation in filtering out tokens that behave differently from standard ERC-20 Tokens. The analysis is under constant improvement and can provide wrong information.
{% endhint %}

## API Documentation

This section documents Tycho's RPC API. Full swagger docs are available at: <a href="https://tycho-beta.propellerheads.xyz/docs/" target="_blank" rel="noopener noreferrer">https://tycho-beta.propellerheads.xyz/docs/</a>

{% openapi-operation spec="tycho-api" path="/v1/health" method="get" %}
<a href="https://gitbook-x-prod-openapi.4401d86825a13bf607936cc3a9f3897a.r2.cloudflarestorage.com/raw/360443387f3e482f98de29f904ec948275acf647b29df78a49023c8ddbde374c.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Content-Sha256=UNSIGNED-PAYLOAD&X-Amz-Credential=dce48141f43c0191a2ad043a6888781c%2F20250715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20250715T115806Z&X-Amz-Expires=172800&X-Amz-Signature=1f529187b381b4605b10bf31cea85aada990553563e076d9b2de1e20737fd0fe&X-Amz-SignedHeaders=host&x-amz-checksum-mode=ENABLED&x-id=GetObject" target="_blank" rel="noopener noreferrer">OpenAPI tycho-api</a>
{% endopenapi-operation %}

{% openapi-operation spec="tycho-api" path="/v1/protocol_components" method="post" %}
<a href="https://gitbook-x-prod-openapi.4401d86825a13bf607936cc3a9f3897a.r2.cloudflarestorage.com/raw/360443387f3e482f98de29f904ec948275acf647b29df78a49023c8ddbde374c.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Content-Sha256=UNSIGNED-PAYLOAD&X-Amz-Credential=dce48141f43c0191a2ad043a6888781c%2F20250715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20250715T115806Z&X-Amz-Expires=172800&X-Amz-Signature=1f529187b381b4605b10bf31cea85aada990553563e076d9b2de1e20737fd0fe&X-Amz-SignedHeaders=host&x-amz-checksum-mode=ENABLED&x-id=GetObject" target="_blank" rel="noopener noreferrer">OpenAPI tycho-api</a>
{% endopenapi-operation %}

{% openapi-operation spec="tycho-api" path="/v1/protocol_state" method="post" %}
<a href="https://gitbook-x-prod-openapi.4401d86825a13bf607936cc3a9f3897a.r2.cloudflarestorage.com/raw/360443387f3e482f98de29f904ec948275acf647b29df78a49023c8ddbde374c.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Content-Sha256=UNSIGNED-PAYLOAD&X-Amz-Credential=dce48141f43c0191a2ad043a6888781c%2F20250715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20250715T115806Z&X-Amz-Expires=172800&X-Amz-Signature=1f529187b381b4605b10bf31cea85aada990553563e076d9b2de1e20737fd0fe&X-Amz-SignedHeaders=host&x-amz-checksum-mode=ENABLED&x-id=GetObject" target="_blank" rel="noopener noreferrer">OpenAPI tycho-api</a>
{% endopenapi-operation %}

{% openapi-operation spec="tycho-api" path="/v1/protocol_systems" method="post" %}
<a href="https://gitbook-x-prod-openapi.4401d86825a13bf607936cc3a9f3897a.r2.cloudflarestorage.com/raw/360443387f3e482f98de29f904ec948275acf647b29df78a49023c8ddbde374c.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Content-Sha256=UNSIGNED-PAYLOAD&X-Amz-Credential=dce48141f43c0191a2ad043a6888781c%2F20250715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20250715T115806Z&X-Amz-Expires=172800&X-Amz-Signature=1f529187b381b4605b10bf31cea85aada990553563e076d9b2de1e20737fd0fe&X-Amz-SignedHeaders=host&x-amz-checksum-mode=ENABLED&x-id=GetObject" target="_blank" rel="noopener noreferrer">OpenAPI tycho-api</a>
{% endopenapi-operation %}

{% openapi-operation spec="tycho-api" path="/v1/tokens" method="post" %}
<a href="https://gitbook-x-prod-openapi.4401d86825a13bf607936cc3a9f3897a.r2.cloudflarestorage.com/raw/360443387f3e482f98de29f904ec948275acf647b29df78a49023c8ddbde374c.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Content-Sha256=UNSIGNED-PAYLOAD&X-Amz-Credential=dce48141f43c0191a2ad043a6888781c%2F20250715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20250715T115806Z&X-Amz-Expires=172800&X-Amz-Signature=1f529187b381b4605b10bf31cea85aada990553563e076d9b2de1e20737fd0fe&X-Amz-SignedHeaders=host&x-amz-checksum-mode=ENABLED&x-id=GetObject" target="_blank" rel="noopener noreferrer">OpenAPI tycho-api</a>
{% endopenapi-operation %}

{% openapi-operation spec="tycho-api" path="/v1/contract_state" method="post" %}
<a href="https://gitbook-x-prod-openapi.4401d86825a13bf607936cc3a9f3897a.r2.cloudflarestorage.com/raw/360443387f3e482f98de29f904ec948275acf647b29df78a49023c8ddbde374c.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Content-Sha256=UNSIGNED-PAYLOAD&X-Amz-Credential=dce48141f43c0191a2ad043a6888781c%2F20250715%2Fauto%2Fs3%2Faws4_request&X-Amz-Date=20250715T115806Z&X-Amz-Expires=172800&X-Amz-Signature=1f529187b381b4605b10bf31cea85aada990553563e076d9b2de1e20737fd0fe&X-Amz-SignedHeaders=host&x-amz-checksum-mode=ENABLED&x-id=GetObject" target="_blank" rel="noopener noreferrer">OpenAPI tycho-api</a>
{% endopenapi-operation %}

