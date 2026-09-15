# Mineral Publisher

Mineral Publisher 是一个面向 Markdown / Obsidian 知识库的自动化内容发布审核服务。

主要流程：

Source
→ Snapshot
→ ChangeSet
→ Policy
→ Review
→ Projection
→ PublishPlan
→ Publisher

项目目标：

- 自动化发布
- 私有内容过滤
- AI 审核
- 人工兜底审核
- Markdown 静态资源依赖处理
- Git 发布历史
- 全流程可追溯

## CLI

CLI 可执行文件名为 `mineral`，默认读取当前目录的 `mineral.yaml`：

```bash
cargo run --bin mineral -- init
cargo run --bin mineral -- doctor
cargo run --bin mineral -- publish
cargo run --bin mineral -- review list
cargo run --bin mineral -- review show document:123
cargo run --bin mineral -- review approve document:123
cargo run --bin mineral -- publish
cargo run --bin mineral -- status
```

使用其他配置文件时，在命令前传入 `--config PATH`。`init` 会创建示例配置、源目录、CAS
和各持久化 schema，但不会覆盖已有配置。`doctor` 只做读取和存在性检查；API key 只报告
`present`/`missing`。R2 与 DeepSeek 凭据直接写在配置文件的 `secret_access_key` / `api_key`
字段中；缺少直接值时固定回退到 `MINERAL_R2_SECRET_ACCESS_KEY` 和
`MINERAL_DEEPSEEK_API_KEY`，不再从配置文件读取环境变量名称。
默认 Markdown 与资源模型均为 `deepseek-flash`。审核请求分别使用可配置的
`markdown_concurrency`（默认 4）和 `asset_concurrency`（默认 2）进行有界并发；stderr
持续显示阶段与单文件进度，stdout 保留稳定的最终报告。相同 Snapshot、内容与审核合同的
已验证结果会直接复用。

人工审核 ID 显式包含 subject 类型（`document:ID` 或 `asset:ID`），避免两类 ID 空间产生
歧义。人工决定与 exact automatic review 绑定；后续 `publish` 会自动发现对应决定。

## 当前实现边界

当前实现覆盖本地 Source、不可变 Snapshot/CAS、确定性隐私与引用检查、Markdown 和图片
语义审核、人工兜底、JPEG/PNG 清理、最终资源闭包、完整 PublicProjection、PublishPlan，
以及具有 compare-and-swap、Noop、Conflict 和 Indeterminate 语义的 Git 发布。V1 不包含
daemon、scheduler、Web UI、OCR、PDF 发布、自动建分支或多目标发布。

## 文档

- 架构设计：`docs/architecture.md`
- Agent 开发约束：`AGENTS.md`

## 开发

项目当前使用 Rust 2024 edition。

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```
