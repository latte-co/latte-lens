# Content search performance check

Text search builds its two capped candidate inventories lazily, after the first
text query. An accepted workspace refresh invalidates the inventory, but does
not rebuild it until the next text query. Queries then reuse the selected
inventory; repeated queries do not traverse the workspace.

Text-search results are a snapshot of the workspace at the last successful
**Refresh**. Changes made afterward are not included until Refresh is run
again; the text-search header is labeled `last Refresh`.

To collect a reproducible local timing over 2,000 small source files, run:

```sh
cargo test search_inventory_timing --lib -- --ignored --nocapture
```

The test compares ten traversal-equivalent candidate-discovery-plus-match
passes with ten warm inventory-only match passes over the identical fixture.
Timing is intentionally informational: disk cache, filesystem, and CPU
concurrency vary by host. The functional assertions in `src/search.rs` cover
ordering, result caps, ignore parity, cancellation, and reindexing.
