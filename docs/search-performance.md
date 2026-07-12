# Content search performance check

Text search builds two capped candidate inventories when the search runtime
starts and after an accepted workspace refresh. Queries reuse the selected
inventory; they do not traverse the workspace.

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
