use crate::encoding::{
    errors::EncodingError,
    models::{EncodedSolution, Solution},
};

/// A high-level interface for encoding solutions into Tycho-compatible transactions or raw call
/// data.
///
/// This trait is designed to abstract the encoding logic required to prepare swap transactions for
/// the Tycho Router. It enables modular and customizable construction of transactions, allowing
/// integrators to maintain full control over the execution constraints.
///
/// # User Responsibility
///
/// While this trait provides convenience methods, it is **strongly recommended** that users favor
/// [`encode_solutions()`](Self::encode_solutions) over `encode_calldata`. This is because:
///
/// - `encode_solutions` returns raw [`EncodedSolution`] objects, which include Tycho’s swap path
///   encoding, but leave **function argument encoding entirely in the user’s hands**.
/// - The function arguments to the router (e.g., `expectedAmountOut`, `minAmountOut`, `receiver`,
///   `permit2`, etc.) are used as **guardrails** to ensure safe on-chain execution.
/// - Automatically constructing full transactions via `encode_calldata` can obscure these important
///   safeguards and may result in unexpected behavior or vulnerability to MEV.
///
/// Tycho is only responsible for generating the internal swap plan. **The user must encode the
/// outer function call arguments themselves** and verify that they enforce correct and secure
/// behavior.
pub trait TychoEncoder: Send + Sync {
    /// Encodes a list of [`Solution`]s into [`EncodedSolution`]s, which include the function
    /// signature and internal swap call data.
    ///
    /// This method gives users maximum flexibility and control. It **does not** produce full
    /// transaction objects. Users are responsible for:
    /// - Constructing the full calldata using their own encoding logic.
    /// - Managing execution-critical parameters like `expectedAmountOut` and `minAmountOut`.
    ///
    /// # Returns
    /// A vector of encoded solutions, each containing:
    /// - The Tycho method function signature
    /// - The encoded swap path
    /// - Additional metadata (e.g., permit2 information)
    ///
    /// # Recommendation
    /// Use this method if you care about execution safety and want to avoid surprises.
    fn encode_solutions(
        &self,
        solutions: Vec<Solution>,
    ) -> Result<Vec<EncodedSolution>, EncodingError>;

    /// Performs solution-level validation and sanity checks.
    ///
    /// This function can be used to verify whether a proposed solution is structurally sound and
    /// ready for encoding.
    ///
    /// # Returns
    /// - `Ok(())` if the solution is valid.
    /// - `Err(EncodingError)` if the solution is malformed or unsupported.
    fn validate_solution(&self, solution: &Solution) -> Result<(), EncodingError>;
}
