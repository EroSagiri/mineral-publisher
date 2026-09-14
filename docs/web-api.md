# S8.2 — Local HTTP API

`mineral web` 起一个**只监听 loopback** 的本地管理 API。它是与 CLI 同级的第二个入口，
两者都只是 `operations/` + `application/` 的 adapter：

```text
        CLI                    Web（本阶段）
         │                      │
         └──────────┬───────────┘
                    ▼
              operations/          start / watch / read
                    ▼
              application/         一次 use case 一个模块
                    ▼
        runtime/ ──> core + adapters
```

启动：

```bash
mineral web                       # 默认 127.0.0.1:8787
mineral web --bind 127.0.0.1:9000 # 换端口
```

## 安全模型（本阶段最重要的约束）

这个接口能 **publish / backup / 批准内容**，所以：

```text
默认 bind = 127.0.0.1:8787
```

不是 `0.0.0.0`。`--bind` 可以覆盖，但覆盖时会明确警告：

```text
warning: binding 0.0.0.0:8787, which is not loopback.
         This interface can publish, back up and approve content, and it has no
         authentication or TLS. Only do this on a network you trust.
```

MVP 明确是**本机管理界面，不是远程管理服务**，因此这一轮不做：账号系统、session、
OAuth、TLS、多用户授权。这样也不会不小心把 `POST /reviews/.../approve` 暴露给局域网。

## 端点

读操作（结构化 DTO，绝不返回 CLI 文本）：

```http
GET /api/v1/status
GET /api/v1/doctor
GET /api/v1/reviews
GET /api/v1/reviews/:attempt
```

长时间 mutation（立即返回 `202`）：

```http
POST /api/v1/operations/publish
POST /api/v1/operations/backup
POST /api/v1/operations/backup/init
POST /api/v1/operations/backup/verify
POST /api/v1/reviews/:attempt/approve
POST /api/v1/reviews/:attempt/reject
```

```json
202 Accepted
{ "operation_id": "op-1", "kind": "publish", "state": "queued" }
```

Operation 状态与事件：

```http
GET /api/v1/operations          # 摘要列表
GET /api/v1/operations/:id      # 权威快照
GET /api/v1/operations/:id/events   # SSE
```

`:attempt` 就是 `document:42` / `asset:7` 原样放在路径段里。

### 人审决策也走 supervisor

`approve` / `reject` **不**直接调 application，而是作为 `review_decision` operation 进入
同一个闸门。否则很快就会出现"CLI 有 gate、Web 没有"的分叉：

```json
409 Conflict
{ "code": "workspace_busy", "message": "workspace is busy with op-1",
  "active_operation_id": "op-1", "active_operation_kind": "backup" }
```

## SSE

```http
GET /api/v1/operations/:id/events
```

```text
id: 17
event: progress
data: {"sequence":17,"kind":"stage","message":"[2/4] Running privacy, ..."}

event: completed
data: {"state":"succeeded"}

```

- `id` 就是 `ProgressEvent.sequence`，所以断线重连可以带 `Last-Event-ID: 17`，
  只重放 `sequence > 17`（`OperationSupervisor::subscribe_after`，这是 operation 层的通用能力，
  SSE 逻辑没有侵进去）。
- 收到 terminal event（`completed` / `failed`）后**流就结束**。事件流只是通知；
  权威状态永远以 `GET /operations/:id` 为准 —— UI 不应该靠最后一条 progress 文本猜结果。

## 错误

```text
OperationErrorCode  ──>  WebApiError { code, message, causes, ... }
```

`code` 稳定、供程序 switch；`message` 只供显示。HTTP status 由 code 在一处映射：

| code | status |
|---|---|
| `invalid_request` | 400 |
| `review_not_found` / `operation_not_found` / `endpoint_not_found` | 404 |
| `workspace_busy` / `workspace_not_configured` / `review_conflict` / `backup_no_base_commit` | 409 |
| `credential_missing` | 503 |
| `connection_failed` | 502 |
| 其余（`publication_failed` / `backup_failed` / `verification_failed` / `diagnosis_failed` / `operation_panicked` / `configuration_invalid`） | 500 |

`405` 由 axum 按协议返回（例如 `GET /operations/backup` 这条只接受 POST 的路径）。

## DTO 边界

**不直接序列化内部类型。** `PublishRun` / `ReviewRun` / `OperationSnapshot` 都是实现细节；
每个响应都在 `web/dto.rs` 手写成 `Web*` 类型，那里是唯一读内部结构来填字段的地方。
内部字段改名因此不会变成 Web breaking change。

两条附带的性质：

- **没有任何字段能携带 secret。** 不是靠 handler 记得删，而是响应类型里根本没有那种字段。
  测试 `no_response_can_carry_a_secret` 递归检查所有响应的 key 不得含
  `secret` / `token` / `api_key` / `credential` / …，并断言凭据值不出现在任何字符串里。
- `backup.configured` 之类只报**存在性**，不报内容。

## Operation 不持久化（明确语义）

```text
浏览器刷新
  → 可以恢复：server 进程还在 + 前端保留 OperationId
  → supervisor 会 replay 全部历史

Mineral 进程重启
  → operation record 丢失
  → GET /operations/:id 返回 404 operation_not_found
  → UI 回落到 durable 状态：/status、review queue、PublishRun 等
```

这是第一版**明确允许**的语义。真正的审计事实由 durable model 承担
（`PublishRun` / `ReviewRun` / `HumanReview` / `RemoteObservation` / `DeliveryProjection`），
operation 只是"某个前端发起的一次正在执行的应用调用及其实时进度"。

保留策略有界：**running 的永远保留，finished 只留最近 100 条**
（`DEFAULT_COMPLETED_OPERATIONS`）。淘汰最旧的，所以一个 id 要么还在，要么是 typed 404，
永远不会变成另一个 operation。

`operation_not_found` 因此同时意味着"不存在 / 已过期 / server 重启过"，
UI 遇到它不要猜状态，直接重新拉 durable 状态。

## 这一轮刻意不做

```text
❌ 前端
❌ operation 持久化
❌ 远程认证 / session / TLS
❌ 配置编辑 / secret 编辑
❌ 多 workspace
❌ WebSocket（SSE 够用：命令走普通 HTTP，进度走单向流）
❌ 拆新 crate（web 就在 mineral-host 里，和 cli 对称）
```

## 验收

```bash
cargo test -p mineral-publisher --lib web::tests    # 18 个
./scripts/verify-cli-independence.sh                # 删掉 cli/ 后 application + operations + web 全绿
```

| 验收点 | 证明 |
|---|---|
| 默认只监听 loopback | `the_default_bind_is_loopback_only`、`the_api_answers_on_a_real_loopback_socket` |
| 真 socket 上可达且响应是 JSON | `the_api_answers_on_a_real_loopback_socket` |
| status / doctor / reviews 结构化 | `status_is_structured`、`the_status_shape_is_the_contract`、`doctor_is_structured`、`reviews_are_structured` |
| POST 立即 202 返回 id | `starting_an_operation_returns_an_identity_immediately` |
| 第二个 mutation → 409，不进引擎 | `a_second_mutation_is_a_409_and_never_reaches_the_engine`（断言 executor 只被进入一次） |
| 人审决策也受闸门约束 | `a_review_decision_takes_the_workspace_gate` |
| 失败带稳定 code + message + causes | `a_failed_operation_reports_a_code` |
| 未知 / 非法 operation id | `operation_identities_are_checked` |
| SSE 带 id、terminal 后结束 | `events_are_streamed_and_end_after_a_terminal_event` |
| Last-Event-ID 只重放未见的 | `events_honour_last_event_id` |
| 任何响应都带不了 secret | `no_response_can_carry_a_secret` |
| 未知端点 / 错误方法 | `an_unknown_endpoint_is_json` |
| CLI 行为与退出码不变 | 真机核对（下表） |

### 真机核对（真实工作区 `mineral.toml`）

```text
GET  /api/v1/status    → 200，结构化：source.kind=r2、backup.configured=true
GET  /api/v1/doctor    → 200，7 passed / 2 failed（R2 与 DeepSeek 凭据未导出）
GET  /api/v1/reviews   → 200，空队列
GET  /api/v1/operations→ 200，[]
GET  /api/v1/operations/op-9        → 404 operation_not_found
POST /api/v1/reviews/document:999/approve → 202 {operation_id:"op-1",kind:"review_decision"}
GET  /api/v1/operations/op-1        → failed, code=review_not_found（typed）
GET  /api/v1/operations/op-1/events → event: failed / data: {"state":"failed"}，然后流结束
```

CLI 侧同时核对：`status`、`doctor`（exit 0/1 与文本不变）、`help`（新增一行 `web [--bind ADDRESS]`）。

## 磁盘

加入 axum + tokio 后，`target/debug` 的 debug info 一度把 50G 磁盘打满两次。
workspace 现在设了：

```toml
[profile.dev]
debug = "line-tables-only"       # 自己的 crate 保留 file:line（backtrace 可读）

[profile.dev.package."*"]
debug = false                    # 依赖不带 debug info
```

panic 仍然报告准确的文件与行号；代价是不能在调试器里看依赖的变量。
这样 `target/debug` 从约 10.5G 降到可接受范围。

## 下一步（S8.3）

Web UI MVP 直接消费这一层，建议顺序：Dashboard（status）→ Reviews（列表 / 详情 /
approve / reject）→ Operations（进度 + 结果）→ Publish 按钮 → Backup → Doctor。
配置编辑留到 S8.4（需要先有权限模型），第一版配置只读。
