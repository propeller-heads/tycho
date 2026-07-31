# Best Practices

Some best practices we encourage on all integrations are:

* **Clear Documentation:** \
  Write clear, thorough comments. Good documentation:
  * Helps reviewers understand your logic and provide better feedback
  * Serves as a guide for future developers who may adapt your solutions
  * Explains _why_ you made certain decisions, not just what they do
*   **Module Organisation:**\
    For complex implementations it is recommended to:

    * Break large `module.rs` files into smaller, focused files
    * Place these files in a `modules` directory
    * Name files clearly with numerical prefixes indicating execution order (e.g., `01_parse_events.rs`, `02_process_data.rs`)
    * Use the same number for parallel modules that depend on the same previous module

    &#x20;A good example of this done well is in the <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/protocols/substreams/ethereum-uniswap-v4/src/modules" target="_blank" rel="noopener noreferrer">uniswap-v4 implementation</a>.
*   **Substream Initial Block:**\
    Your package will work just fine setting the initial block in your manifest file to `1`, however it means anyone indexing your protocol has to wait for it to process an excessive number of unnecessary blocks before it reaches the first relevant block. This increases substream costs and causes long wait times for the protocol to reach the current block.

    A good rule of thumb is to identify the earliest deployed contract that you index and set this config to that block.
* **Performance Considerations:**
  * Minimize use of `.clone()`, especially in loops or on complex/nested data structures. Instead use references (`&`) when possible.
