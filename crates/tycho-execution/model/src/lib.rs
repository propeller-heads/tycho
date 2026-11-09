//! # Run
//!
//! Run the model using:
//! ```text
//! cargo run --release
//! ```
//!
//! `--release` is important. It's very slow otherwise.
//!
//! # Motivation
//!
//! This model was created during a security assessment of Tycho Router V3.
//!
//! Tycho Router V3 allows users to execute complex swap sequences
//! in which the users have control over many parameters.
//! These swap sequences can transfer assets out of the router, give allowance
//! to spend the router's assets,
//! and increase balances, which allow withdrawal of assets from the router.
//! At the same time, the router acts as a vault that can hold potentially significant amounts of
//! user's assets.
//! This functionality was added with V3 and allows users to efficiently
//! use those assets using swap.
//! Earlier security assessments warned that assets owned by the router
//! must not be considered safe.
//! As a result, Tycho Router V3 is a risky product with a large attack surface.
//!
//! During the security assessment the author found a two similar critical vulnerabilities
//! that allowed an attacker to steal all assets owned by the router.
//!
//! The author realized that, due to the large amount of user controlled parameters,
//! manual review would not be sufficiently thorough in finding similar potential
//! vulnerabilities.
//!
//! This motivated the construction of a model of Tycho Router V3's functionality in Python.
//!
//! Simulating many relevant parameter combinations in the model
//! confirmed the two critical vulnerabilities and discovered two additional critical
//! vulnerabilities.
//!
//! As the model was extended, the Python implementation took too long to run.
//! The model was rewritten in Rust with a focus on performance.
//!
//! As the model was extended to two sequential swaps, the Rust implementation
//! was [optimized](#optimization)
//! and [parallelized](#parallelism).
//!
//! # Limitations
//!
//! The model has been effective in discovering several critical vulnerabilities.
//!
//! If the model doesn't find any suspicious outcomes,
//! that doesn't mean that no vulnerabilities exist.
//!
//! It means that, assuming the model is correct,
//! there are no outcomes that trigger the [suspicious outcome detector](Outcome::is_suspicious)
//! that lie within the [modeled functionality](model) and within the simulated parameter space.
//!
//! In other words, whether the model can detect a vulnerability depends on the following:
//!
//! 1) The model correctly models the original Solidity code
//! 2) The [suspicious outcome detector](Outcome::is_suspicious) can detect the vulnerability
//! 3) The vulnerability lies within the modeled functionality
//! 4) The parameters that cause the vulnerability lie within the simulated parameter space
//!
//! Only the subset of [Executor](model::executors::Executor)s that give the caller control
//! over the called pool contract were modeled.
//! These [Executor](model::executors::Executor)s present a higher risk than those that interact
//! with a protocol contract the caller has no control over.
//! They are also far easier to model.
//!
//! Sequential swaps and split swaps with more than 2 swaps were not simulated.
//!
//! At the time of the hand over, this project is intended to model
//! <https://github.com/propeller-heads/tycho-execution/tree/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry>.
//! Later commits are not yet reflected in the model.
//!
//! # Highlevel Overview
//!
//! [simulate], the project's most important function, takes [Params](params::Params)
//! and runs the [model] of TychoRouterV3.
//!
//! [model] mostly follows the naming and file structure of the original Solidity TychoRouterV3.
//!
//! [Params](params::Params) represents all parameters the execution depends on,
//! like whether to call `singleSwap` or `sequentialSwap`, each swap's
//! [Executor](model::executors::Executor), the swap count, `token_in`, each swap's protocol data,
//! the `receiver`, etc.
//!
//! The list of work, [Params](params::Params) to try, is not known upfront.
//! While it would be possible to enumerate every single possible [Params](params::Params)
//! in an outer loop, this would be incredibly wasteful and lead to code that
//! is harder to understand.
//! For example, whether the [FluidV1](model::executors::Executor::FluidV1) executor's
//! `is_native_sell` parameter is `true` or `false`, would have to be enumerated for
//! every executor, including those where the parameter doesn't make a difference. This would double
//! the time needed. That's just for one parameter. There are many more and most are not just simple
//! binary choices.
//!
//! Instead, additional parameters can be [requested](params::Params::request) anywhere
//! in the model exactly where they are needed, which is easy to understand, and only when they are
//! needed, which is efficient.
//!
//! As a result, we're dealing with a problem where work can generate more work.
//!
//! If all work were known upfront, then it could be evenly partitioned among all
//! [worker_thread](worker::worker_thread)s which would not need to interact.
//!
//! Since work can generate more work some interaction between
//! [worker_thread](worker::worker_thread)s
//! is required.
//! [work-stealing](worker::find_work) parallel computation was chosen because
//! it keeps expensive interaction to a minimum.
//!
//! [worker_thread](crate::worker::worker_thread) [finds work](crate::worker::find_work),
//! calls [simulate], handles [Error]s, resolves [RequestParam](params::RequestParam)s,
//! checks whether successful runs result in [suspicious](Outcome::is_suspicious)
//! outcomes, and writes suspicious [Outcome]s as YAML docs to stdout.
//!
//! # Recommendations
//!
//! 1) Double-check that the model correctly mirrors the essential functionality of the Solidity
//!    code
//! 2) Add tests to reinforce 1. and to reduce the probability of dangerous false negatives, i.e.
//!    vulnerabilities that are hidden due to bugs in the model
//! 3) Model the remaining [Executor](model::executors::Executor)s
//! 4) Reduce the amount of user inputs, i.e. parameters the caller can control, to reduce attack
//!    surface and reduce the time the model takes to run
//!
//! # Parallelism
//!
//! The main function spawns [WORKER_THREAD_COUNT](config::WORKER_THREAD_COUNT)
//! [worker_thread](crate::worker::worker_thread)s.
//!
//! Initially a single global FIFO-queue behind a [Mutex](std::sync::Mutex) was used.
//! This resulted in no speedup using eight cores compared to a single core.
//! The [worker_thread](crate::worker::worker_thread)s spent most of their time waiting on the
//! [Mutex](std::sync::Mutex).
//!
//! Implementing efficient [work-stealing](worker::find_work)
//! parallel computation caused the model to scale with the number of CPU cores.
//!
//! # Optimization
//!
//! Because the project relies heavily on hash-maps,
//! replacing [std::collections::HashMap] with [rustc_hash::FxHashMap]
//! improved performance by more than 100%.
//! [rustc_hash::FxHashMap] uses a faster hash function that is vulnerable to
//! DOS attacks. If [rustc_hash::FxHashMap]'s key is user input, the user
//! can craft malicious inputs that make [rustc_hash::FxHashMap] perform very slowly
//! and much slower than [std::collections::HashMap].
//!
//! Since the project doesn't have user inputs and since performance matters a lot,
//! [rustc_hash::FxHashMap] was chosen.
//!
//! In general, using [std::collections::HashMap] is recommended.
//! [std::collections::HashMap] is fast enough for most applications and not vulnerable to DOS.
//!
//! Other optimizations include [ParamKey](params::ParamKey) and
//! [LazyAtomicCounter](progress::LazyAtomicCounter).
//!
//! # Disclaimer
//!
//! This model was created by a human expert without the use of generative AI.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
//! INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//! IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM,
//! DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

mod address;
pub use address::Address;
pub mod config;
mod error;
pub use error::Error;
pub mod log;
pub mod math;
pub mod model;
mod outcome;
pub use outcome::Outcome;
pub mod params;
mod simulate;
pub use simulate::simulate;
mod state;
pub use state::State;
mod telemetry;
pub use telemetry::Telemetry;
pub mod progress;
pub mod worker;
