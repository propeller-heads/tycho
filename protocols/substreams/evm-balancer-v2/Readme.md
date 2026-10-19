# Balancer Substream

## Open tasks

### Missing rate provider state

Any pool that does use rate providers, is currently not supported by tycho since we do
not witness the contract creation of rate providers and thus can't provide the required
contract state.

This is planned to be resolved with the dynamic contract indexing module.

## Static Attributes

| name               | type  | description                                                                                             |
|--------------------|-------|---------------------------------------------------------------------------------------------------------|
| pool_type          | str   | A unique identifier per pool type. Set depending on the factory                                         |
| normalized weights | json  | The normalised weights of a weighted pool.                                                              |
| pool_id            | str   | A hex encoded balancer pool id.                                                                         |
| rate_providers     | json  | A list of rate provider addresses.                                                                      |
| bpt                | bytes | The balancer lp token, set if the pool support entering and exiting lp postions via the swap interface. |
| main_token         | bytes | The main token address for a linear pool                                                                |
| wrapped_token      | bytes | The wrapped token address for a linear pool                                                             |
| fee                | int   | The fee charged by the pool set at deployment time                                                      |
| upper_target       | int   | The upper target for a linear pool                                                                      |