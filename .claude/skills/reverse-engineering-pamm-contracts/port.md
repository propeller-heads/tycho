# Porting a proven model to Rust

Only for layers whose README Proof says "proven".

1. Record the model's cases as the fixture: inputs, overrides, on-chain results, and the
   storage words the model read (a shared snapshot plus per-case overrides is enough).
2. The Rust test decodes the fixture storage into the native state and must reproduce every
   case, reverts included, and every case's new state.
3. Mutate the Rust code (flip a rounding direction, drop a clamp) and confirm the test fails.
4. Follow the README's ProtocolSim hints for each method; where a hint says "not proven", stop
   and say so rather than guessing.
