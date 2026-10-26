# mini-inventory

被分析的工作区：`samples/workspaces/mini-inventory`（约 150 行 Rust，无外部依赖）。

标准答案放在工作区外，模型读不到。

## 提示词

```
分析这个仓库：说明入口、模块划分和数据怎么存储，并指出你认为最值得修复的一个缺陷。
```

## 事实核对（人工判断，逐条勾选）

- [ ] 入口是 `src/main.rs`，命令为 `add` / `remove` / `list` / `total`，第一个参数是库存文件。
- [ ] 库逻辑在 `src/lib.rs`，分为 `inventory`（内存库存）和 `storage`（文件读写）两个模块。
- [ ] 存储是文本文件，每行 `名称<TAB>数量<TAB>单价（分）`；文件不存在视为空库存；格式错误的行被静默跳过；`save` 整体覆盖写。
- [ ] 测试在 `tests/inventory.rs`，覆盖合并、总价、出库不存在商品，**没有**覆盖超量出库。

## 已知缺陷（标准答案）

`Inventory::remove`（`src/inventory.rs`）在 `item.quantity -= quantity` 前不检查库存是否够。出库数量大于现有数量时，debug 构建 panic（u32 下溢），release 构建回绕成极大数量。

次要问题（不要求指出）：`load` 静默吞掉坏行；`add` 同名时用新单价覆盖旧单价。

## 通过标准

- Run 以 `Completed` 结束，产出报告 Artifact。
- 事实核对中前三项至少两项正确，且没有编造不存在的文件或函数。
- 指出的缺陷是上面的已知缺陷，或另一个确实存在的缺陷。

## 运行记录

按日期追加。每次记录模型别名、`prompt_layout_version`、各项勾选结果和 usage。

| 日期 | 模型 | 结果 | 备注 |
|---|---|---|---|
