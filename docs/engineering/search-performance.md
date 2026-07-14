# 内容搜索性能检查

Text search 在第一次文本查询后才延迟构建两组有上限的候选集。一次成功的 workspace
Refresh 会使候选集失效，但不会立即重建；直到下一次文本查询才重新构建。后续查询会
复用选中的候选集，重复查询不会重新遍历工作区。

Text search 结果是最近一次成功 **Refresh** 时的工作区快照。之后发生的变更要等到
再次 Refresh 才会进入结果；text search 标题会显示 `last Refresh`。

File search 和 text search 分别维护独立的内存 popup session。隐藏 popup 或打开其中
一个结果后，下次打开时仍保留该模式的 query、results、selected item 和 list
position。`Ctrl+U` 和可点击的 `Clear` 会显式重置当前查询。成功 Refresh 会使已保存
结果失效；重新打开非空的已保存 text query 时，会基于新快照重新执行查询，并在原
identity 仍存在时恢复先前结果。

如需在 2,000 个小型源码文件上采集可复现的本地耗时，运行：

```sh
cargo test search_inventory_timing --lib -- --ignored --nocapture
```

该测试在同一个 fixture 上比较十次“候选发现加匹配”的等价遍历，与十次 warm
inventory-only 匹配。耗时只作为信息记录：不同主机的磁盘缓存、文件系统和 CPU 并发
情况会产生波动。`src/search.rs` 中的功能断言负责覆盖排序、结果上限、ignore 语义
一致性、取消和重新索引。
