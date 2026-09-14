# Host 分层重构（CLI 与 Web UI 同级）

目标：把 `crates/mineral-host/src/cli.rs` 从 "composition root + config + application flow +
presentation 的混合物" 拆成依赖方向明确的分层，让 CLI 与将来的 Web API 成为**同级入口**。

```text
                        mineral-core
                    纯 Domain / 状态机
                           │
                           │ ports
                           ▼
                    Application Layer              一次 use case 一个模块
                 publish / backup / review          输入 Request，输出 Outcome
                  status / doctor / verify          绝不打印、绝不读环境变量
                           │
                 ┌─────────┴─────────┐
                 │                   │
               CLI                 Web API          两个同级入口，只做
           parse + render       parse + JSON        “解析 → 调用 → 呈现”
                 │                   │
                 └─────────┬─────────┘
                           │
                    Runtime / Host                  composition root：
             R2 / Git / SQLite / AI / LFS           谁被构造、用哪个实现
```

禁止的依赖方向：

```text
Web → CLI → Host             ✗  Web 绝不复用 CLI 逻辑
Application → CLI            ✗  Application 不认识参数解析或输出格式
Application → std::env / println ✗  凭据走 SecretProvider，输出走调用方
```

## 目标模块布局

```text
crates/mineral-host/src/
├── config/                 文件 → RawConfig → ValidatedConfig → Secrets
│   ├── model.rs            serde 模型、默认值、示例配置（TOML / YAML）
│   ├── load.rs             读文件、按扩展名选择格式
│   ├── validate.rs         校验 + 规范化（fail closed，顺序与错误信息稳定）
│   ├── secrets.rs          SecretName / SecretValue / SecretProvider
│   ├── tests.rs
│   └── mod.rs
├── runtime/                composition root
│   ├── workspace.rs        WorkspaceRuntime：配置 + SecretProvider + CAS
│   ├── composition.rs      CAS / 各 SQLite store / source / reviewer / target /
│   │                       git / lfs / 身份分配：谁被构造、用哪个实现
│   ├── progress.rs         Progress / NoProgress / StderrProgress
│   ├── tests.rs
│   └── （bounded / clock / evaluators 为既有实现，未搬动）
├── application/            一次 use case 一个模块，结构化输入输出
│   ├── publish.rs backup.rs review.rs status.rs doctor.rs
│   ├── tests.rs            验收测试：完全不经过 CLI
│   └── mod.rs              ApplicationError（typed）
├── cli/                    同级入口之一：解析 → 调用 → 呈现
│   ├── args.rs             words → Invocation
│   ├── commands.rs         Invocation → application request
│   ├── output.rs           outcome → 文本（本 crate 唯一的 println!）
│   └── mod.rs
└── （现有 adapter 暂不物理搬动）
    asset/ publisher/ reviewer/ runtime/ source/ storage/ backup/
```

`mineral-core` 已经承担 domain，**不**为了目录漂亮再拆新 crate；先固定概念边界与依赖方向。
`adapters/` 只是这棵树里的概念分组，物理位置等依赖方向稳定后再决定。

## 三个配置层

```text
mineral.toml           文件（TOML 为主；.yaml/.yml 仍可读，旧工作区不受影响）
      │  config::load
      ▼
RawConfig              只表达“文件里写了什么”，serde + deny_unknown_fields
      │  config::validate
      ▼
ValidatedConfig        已校验/规范化：绝对路径、ref 合法、namespace 不重叠、必填项
      │  SecretProvider
      ▼
Runtime                环境/凭据解析后的可运行对象
```

`ValidatedConfig` 可以被测试直接构造（不需要文件系统），`Runtime` 才能碰适配器。

## Application API 的形状

```rust
pub struct BackupRequest { /* 目标 + 元数据 + 时间 */ }
pub struct BackupOutcome {
    pub snapshot_id: SnapshotId,
    pub files: usize,
    pub lfs_objects: usize,
    pub run_id: Option<BackupRunId>,
    pub execution: BackupExecutionOutcome,
}

pub fn backup(runtime: &WorkspaceRuntime, request: BackupRequest)
    -> Result<BackupOutcome, BackupError>;
```

- Outcome 承载**渲染所需的一切**，不打印：CLI 渲染成文本，Web 序列化成 JSON，将来 GUI 映射成 UI model。
- Error 尽量 typed（`Configuration` / `Credential` / `Source` / `Lfs` / `Git` / `Review` / `RemoteChanged` …），
  Web 才能映射成 HTTP 状态与 UI 提示，而不是解析错误字符串。
- 进度/阶段信息（如 `[1/4] …`）不进 Outcome：需要时由调用方传 progress sink，或读 Outcome 字段自己呈现。

## 分阶段与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| A | `config/`（model / load / validate / secrets），YAML 保持可用，新增 TOML | 现有测试不改动即通过；新增 TOML/Secret 测试 |
| B | `runtime/`（`WorkspaceRuntime` + composition），`cli.rs` 不再自己 new 适配器 | 现有测试不改动即通过 |
| C | `application/`：publish / backup / review / status / doctor 各自 Request→Outcome，**零打印** | 新增 application 级测试直接调用 use case |
| D | `cli/`：args + commands + output，只做解析与渲染 | CLI 测试改为断言 Outcome；行为不变 |
| E | 验收证明 + 文档 + 全门禁 | 见下 |

**总验收标准（一句话）**：

```text
rm -rf src/cli/
```

之后除了"没有命令行入口"以外，`publish / backup / backup verify / review list|show|approve|reject /
status / doctor` 的全部业务能力仍然存在，并且可以被测试**不经过 CLI** 直接调用。
做到这一步，Web API 就是给现有 Application 层套一层 HTTP 壳。

## 门禁

每个阶段都必须保持：

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo check -p mineral-core --target wasm32-unknown-unknown
```

Public Pipeline（URL rewrite / R2 assets / privacy / review / sanitization / Git CAS）的行为不得改变。

## 交付记录

四个阶段已全部落地，每个阶段各自通过全门禁：

| 阶段 | 提交 | 内容 |
|---|---|---|
| A | `refactor(host): extract the configuration layer from the CLI` | `config/{model,load,validate,secrets}`，TOML 与 YAML 并存，`mineral init --toml` |
| B | `refactor(host): move the composition root into a runtime layer` | `runtime/{workspace,composition}`，CLI 不再构造任何 adapter |
| C+D | `refactor(host): add the application layer and reduce the CLI to an entry point` | `application/{publish,backup,review,status,doctor}`，`cli/{args,commands,output}` |

### Application API 的最终形状

```rust
// 输入是值，时间也是输入，凭据来自 runtime 的 SecretProvider
pub struct PublishRequest { pub created_at: SystemTime, pub human_reviews: ExplicitHumanReviewSelection }
pub struct BackupRequest  { pub created_at: TimestampMillis }

// 输出承载渲染所需的一切
pub fn publish(runtime, PublishRequest, &Arc<dyn Progress>) -> Result<PublishOutcome, ApplicationError>;
pub fn backup (runtime, BackupRequest, &dyn Progress)      -> Result<BackupResult, BackupError>;
pub fn backup_status(runtime) -> Result<BackupStatusOutcome, ApplicationError>;
pub fn backup_verify(runtime) -> Result<BackupVerifyOutcome, ApplicationError>;
pub fn backup_init  (runtime) -> Result<BackupInitOutcome, ApplicationError>;
pub fn review(runtime, &ReviewRequest) -> Result<ReviewOutcome, ApplicationError>;
pub fn status(runtime) -> Result<StatusOutcome, ApplicationError>;
pub fn doctor(runtime) -> Result<DoctorOutcome, ApplicationError>;
```

三个不变量由代码结构保证，而不是靠约定：

1. **零打印。** 整个 binary 的 `println!` 只在 `cli/output.rs`；进度走 `Progress` sink
   （`NoProgress` 给测试，`StderrProgress` 给终端）。
2. **零环境读取。** 应用层不出现 `std::env`；凭据由 `SecretProvider` 解析，
   `SecretProvider: Debug` 且 `StaticSecretProvider` 打印变量名而从不打印值。
3. **typed failure。** `RuntimeError`（Configuration / Credential / Connection）、
   `ApplicationError`（Runtime / Operation / Unsupported / NotFound / Conflict）、
   以及 backup 独有的 `BackupError::NoBaseCommit`——调用方 match 枚举即可，
   不需要解析字符串。

### 验收复现

```bash
./scripts/verify-cli-independence.sh
```

脚本删除 `crates/mineral-host/src/cli/`、把 `main.rs` 打桩（被删除的正是入口本身），
然后跑 `cargo test --workspace --all-targets --all-features` 与
`cargo test -p mineral-publisher --lib application::tests`，结束时无论成败都还原工作树。

实测结果：CLI 删除后 **406 core + 424 host** 测试全绿，其中
`application::tests` 的 8 个用例分别直接调用 publish / backup（含
status、verify、init 的未配置与类型化失败路径）/ review（list、show、
approve 幂等、reject 冲突、未知名与非法名）/ status / doctor，
全部不经过参数解析、不经过打印。

也就是说：**Web UI 只需要给现有 Application 层套一层 HTTP 壳**。
