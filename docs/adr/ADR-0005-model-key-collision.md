# ADR-0005: Model key collision — path-ranked suffixes, never traversal order

- Status: accepted
- Date: 2026-09-19
- Job: M1 / 开发计划 issue 4（Secure model scanner）
- Code: `crates/scanner/src/key.rs`（`base_key` / `build_index` / `model_id`）、
  `crates/scanner/src/lib.rs`（`bookkeeping_for`）
- Tests: `cargo test -p model-serving-scanner`
  （`collision_order_does_not_change_keys`、`keys_for_paths_is_order_independent`、
  `reserved_key_forces_a_suffix`、`collision_suffixes_never_collide_with_reserved_keys`、
  `model_id_is_stable_and_path_specific`）

## Context

`models.key` 是 `UNIQUE` 且是 API 路径里的可读标识（`docs/api.md` §5）。扫描
`/mnt/...` 中多个目录时，不同子目录下同名文件（`a/qwen.gguf` 与 `b/qwen.gguf`）
必然产生同一个 base key，必须有确定的冲突消解规则。三条硬约束：

1. **顺序无关**：`read_dir` 的返回顺序在 Windows/NTFS 与 WSL/`drvfs` 上不同，且同一
   目录两次枚举也可能不同。若按“先到先得”分配 `qwen` / `qwen-1`，同一棵树在两台
   机器或两次扫描上会得到不同 key，索引与 API URL 随之漂移。
2. **跨重启/删除/重现稳定**：daemon 重启、行被软删除后文件重新出现，都必须落回同一
   行（同一 `id`、同一 `key`），否则管理端配置的 default load config 会脱离模型。
3. **不引入依赖**：`docs/architecture.md` §2 未固定 hash/uuid crate，因此稳定 id 用
   自带的 SHA-256 实现（RFC 4122 v5 形式）派生，而不是新增依赖。

## Decision

1. **base key**：文件 stem 小写、非 `[a-z0-9]` 字符压成 `-`（连续 `-` 合并、首尾裁掉），
   结果为空则用 `model`。
2. **无冲突即原样使用**：某 base key 在“本次扫描候选 × 现存索引 key”范围内只出现一次，
   且未被现存行占用，就直接用作 key。
3. **冲突按路径字典序排名**：把声明同一 base key 的候选按其**规范化绝对路径**升序排序，
   依次分配 `<base>-1`、`<base>-2`、… `<base>-n` —— 字典序最小的路径拿到 `-1`，最大的拿到
   `-n`。排名依据是路径本身，不是遍历位置，因此结果与 `read_dir` 顺序完全无关
   （`bookkeeping_for` 另按 key 把“现存行”整组钉住：扫描不去重新生成已被占用的 key，
   例如管理员改过的 key）。
4. **`-<n>` 家族与现存索引一起保留**：`Reserved` 集合同时包含现存行的 key 与本次扫描已
   分配的 key，所以一个现存行持有的 `shared-1` 不会被新文件抢走（它会拿到 `shared-2`），
   同时也杜绝同一次扫描内两个候选算出相同后缀。
5. **id 与 key 解耦**：`id` 只由路径派生（`model_id`），与冲突结果无关；key 冲突只影响
   可读标识，不会让某一行“换身份”。

## Consequences

- 正则顺序（`a` 在前）与逆序（`b` 在前）的两次遍历得到**逐字节相同**的 key 映射：
  测试 `collision_order_does_not_change_keys` / `keys_for_paths_is_order_independent`
  以两种顺序断言同一结果，`collision_suffixes_never_collide_with_reserved_keys` 断言
  现存 `shared` / `shared-1` 把新候选推到 `shared-2`、`shared-3`。
- 冲突候选数量变化（新增一个同名文件）会重排该组后缀：这是可接受的代价（key 是
  可读标识而非主键，`id` 稳定；API 列表以 key 展示，重排后客户端重新拉取即可）。
  若要避免重排，必须引入内容哈希作为 key 的一部分，与约束 3（不新增依赖）冲突。
- 后缀是 `-1`…`-n` 而不是路径哈希，便于管理员在 UI 中辨认；哈希形态（16 hex）在本
  项目里只用于 `id`，不进入 URL。
