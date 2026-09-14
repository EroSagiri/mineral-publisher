# S8.1 — Operation API（CLI 与 Web 之间的那一层）

S7 结束时，`CLI` 与 `Application` 已经是两个同级概念：

```text
CLI ──> Application ──> Runtime / Composition ──> Core + Adapters
```

但 `CLI` 与 `Web` 有一个本质区别：终端可以"启动 → 等几十秒 → 打印进度 → 退出"，
HTTP 不行。Web UI 需要：

```text
点击 Publish → 立刻拿到 operation id → 持续收到进度 → 最终得到结果
```

所以这一层要解决的不是 HTTP routing，而是**把长时间 Application 调用包装成可观察的
Operation**。做完之后：

```text
              CLI        Web (S8.2)
               │            │
               └─────┬──────┘
                     ▼
              operations/            ← S8.1：传输中立
                     ▼
              application/
                     ▼
              runtime/ ──> core + adapters
```

CLI 和 Web 都只是 `operations/` 的 adapter，谁也不复用谁的业务逻辑。

## 模块

```text
crates/mineral-host/src/operations/
├── model.rs        OperationId / Kind / State / Request / ProgressEvent
│                   OperationFailure（typed code + message + cause chain）
│                   OperationResult / OperationSnapshot / StartError
├── executor.rs     OperationExecutor（trait）+ ApplicationExecutor（生产实现）
├── supervisor.rs   OperationSupervisor / Subscription / 单飞闸门
├── tests.rs        17 个验收测试
└── mod.rs
```

`operations/` 里**没有** HTTP status、JSON、SSE、WebSocket、`axum`、`println!`。
S8.2 只负责把这里的值翻译成协议。

## 契约

```rust
let supervisor = OperationSupervisor::new(Arc::new(ApplicationExecutor::new(runtime)));

let id: OperationId = supervisor.start(request)?;   // 立即返回，不等待
let events = supervisor.subscribe(id);              // 进度 + 完成
let snapshot = supervisor.snapshot(id);             // 任意时刻的完整状态
let final_ = supervisor.wait(id);                   // 等它结束（内部就是订阅 + 排空）
```

- `start` 只做两件事：抢闸门、记一条 record、把工作丢到自己的线程。它**不**调用用例。
- `subscribe` 会**重放**已发生的进度，所以"先订阅后启动"和"先启动后订阅"对调用方
  完全一样。事件流以 `Finished { state }` 结束，然后迭代终止。
- `snapshot` 是给 Web 序列化用的：id、kind、state、时间戳、完整进度、结果/失败。
- 结果以 `Arc<OperationResult>` 存放，所以读一份快照不会复制一整条 publication trace。

### 进度复用既有的 `Progress` sink

没有第二套 publish progress：

```text
用例 → Progress sink → RecordProgress → ProgressEvent{sequence, kind, message}
                                     ├─ 订阅者 channel  （CLI 打印 / 未来 SSE）
                                     └─ record 历史      （后到者重放 / snapshot）
```

`kind` 把原先就存在的区别（`stage` vs `detail`）命名出来，渲染方可以区别对待。

## 并发：一个工作区同时最多一个 mutating operation

CLI 时代一个进程一次 publish，Web 时代用户会连点两次，或者 publish 没完就点 backup。
`publish / backup / backup-init / review-decision` 都会写同一批东西
（Git worktree、SQLite、对象存储、远端 ref），所以它们**互斥**：

```rust
match supervisor.start(OperationRequest::Backup(..)) {
    Err(StartError::WorkspaceBusy { active_operation_id }) => { /* 明确拒绝 */ }
    Ok(id) => { /* ... */ }
}
```

拒绝发生在 supervisor 里，**不进入引擎**（测试断言 executor 只被进入一次）。

只读的 `verify_backup` / `doctor` **不抢闸门**：正在 publish 时 `status` / `doctor`
恰恰是最需要能用的，让它们排队毫无意义。

两个由此发现的顺序问题已经修掉：

1. **闸门要在 `Finished` 之前释放。** 否则"等待完成 → 立刻启动下一个"会看到假的
   `WorkspaceBusy`；Web 客户端一定会撞上这个竞态。
2. **用例 panic 不能把工作区锁死。** worker 线程里 `catch_unwind`，把 panic 变成
   `OperationErrorCode::OperationPanicked`，然后正常释放闸门。

## 错误：typed code，而不是解析字符串

```rust
pub enum OperationErrorCode {
    WorkspaceBusy, WorkspaceNotConfigured, ConfigurationInvalid,
    CredentialMissing, ConnectionFailed, InvalidRequest,
    ReviewNotFound, ReviewConflict, BackupNoBaseCommit,
    PublicationFailed, BackupFailed, VerificationFailed,
    DiagnosisFailed, OperationPanicked,
}
```

`code.as_str()` 是**稳定**的 wire 值（`"backup_no_base_commit"`），`message` 是给人看的，
可以随时改。映射只有一处：

```text
ApplicationError ──classify(default, error)──> OperationFailure
     default 来自"跑的是哪个用例"，细化来自 error 自己的 variant
```

`OperationFailure` 是 `Send` 的（否则跨线程就只能退化成字符串），并且保留整条 cause chain。
`to_error()` 把链还原成 `Error::source()`，所以 CLI 打印出的 `caused by:` 与重构前逐字相同。

唯一带结构化数据的失败是 `BackupNoBaseCommit { target, snapshot_id, files, message }`：
"ref 还不存在"是操作员一步就能解决的状态，报告需要点名它冻结了哪个 Snapshot。

## CLI 现在也是 operation 的 adapter

`cli/commands.rs` 的 `operate()`：

```text
start → subscribe → 逐条打印进度到 stderr → 等 Finished → 渲染 typed result
```

行为与退出码逐项核对过（见下），只有一处内部变化：错误先跨线程、再还原成链。

## 验收

```bash
cargo test -p mineral-publisher --lib operations::tests    # 17 个
./scripts/verify-cli-independence.sh                       # 删掉 cli/ 后仍然全绿
```

| # | 验收点 | 证明 |
|---|---|---|
| 1 | 删除 `cli/` 后 application + operations 测试全过 | `verify-cli-independence.sh` |
| 2 | `operations/` 不依赖 `cli/` | 它是 lib 模块；脚本删除 `cli/` 后照常编译 |
| 3 | `start` 立即返回 id | `start_returns_an_identity_before_the_work_finishes` |
| 4 | 进度可由非 CLI sink 收集 | `progress_is_collectable_through_an_operation_subscription`、`a_late_subscriber_replays_the_whole_history` |
| 5 | success / failure 有 typed final result | `a_successful_operation_keeps_a_typed_result`、`a_failed_operation_keeps_a_typed_failure` |
| 6 | 第二个 mutating → `WorkspaceBusy`，不进引擎 | `a_second_mutating_operation_is_refused_as_workspace_busy`（断言 executor 只进入一次） |
| 7 | 只读 status 不需要解析 stdout | `read_only_use_cases_answer_with_values` |
| 8 | 失败保存 typed code，不只有字符串 | `a_failed_operation_keeps_a_typed_failure`（code + message + chain） |
| 9 | core/application/runtime/operations 无 HTTP 类型 | 无 HTTP 依赖；`operations/` 只依赖 std + 本 crate |
| 10 | CLI 行为与退出码不变 | 真机端到端（下表） |

### 真机端到端（criterion 10 的证据）

用一个临时工作区（local source + `git init` 的 origin + 文件系统 asset target，
provider 凭据故意不设）跑完整链路：

| 命令 | 结果 |
|---|---|
| `publish`（首次，已 approve） | stdout `status: published`，远端 ref 前进，tree 含 `note.md`，exit 0 |
| `publish`（再跑一次） | stdout `status: noop`，exit 0 |
| `publish` 的进度 | **stderr**：`[1/4] Creating immutable Snapshot...` / `[1/4] Snapshot … contains 1 files.` / `[2/4] Running privacy, program checks, and semantic review...` / `      Markdown review: note.md` / `      Markdown reviews completed: 1` / `[4/4] Publication workflow finished.` |
| `publish`（provider 未配置） | stdout `status: waiting_for_human_review`，exit 0 |
| `publish`（远端 ref 缺失） | stderr `error: … publication target ref is missing` + 3 层 `caused by:`，exit 1 |
| `review list` / `show` | 与重构前同样的文本 |
| `review approve` | `Review Approve.` exit 0；再次 → `Review already Approve.` exit 0 |
| `backup` / `backup verify` / `backup init`（未配置） | 三行同样的提示，exit 0 |
| `doctor`（有检查失败） | 检查表在 **stdout**，`error: one or more doctor checks failed` 在 stderr，exit 1 |
| 未知命令 / `--config` 缺参 | exit 1 |

## S8.2 会做什么（本阶段刻意不做）

HTTP adapter 应该很薄：`POST /api/v1/operations/publish` 立刻返回 `{"operation_id": …}`，
`GET /api/v1/operations/:id/events` 用 SSE 转发 `ProgressEvent`，
`GET /api/v1/status`、`/reviews`、`/doctor` 直接序列化 application 的 outcome，
`OperationErrorCode` 映射成 HTTP status + `{"code": …}`。

还有两件事本阶段没有做，留给 S8.2/S8.3：

- Operation **不持久化**：id 只在一个 supervisor 的生命周期内有效。跨重启的相关性靠
  引擎自己写的 durable record（publish run / backup run），不靠这个 id。
- 并发目前是"每 supervisor 一个闸门"。多工作区（多个 supervisor）天然并行，
  没有全局锁。
