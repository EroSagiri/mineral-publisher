# Mineral Publisher 双运行时拆分笔记

本文件记录 **core / host 拆分过程中实测得到的事实**，用于支撑架构决策。它不是架构说明（见 `docs/architecture.md`），而是"为什么这样切、代价是什么"的证据记录。

内容分三类：

1. 平台边界判定规则与现状
2. 持久化事务边界的真实形状
3. 内存与二进制解码的实测数据，以及 Worker 运行时的硬约束

---

## 1. 平台边界判定规则

### 1.1 "纯"的判定

一个模块可以进入 `mineral-core`，当且仅当其**非测试代码**不包含：

```text
std::fs / std::process / std::env / std::thread
SystemTime::now() / Instant::now()
reqwest / rusqlite / image / kamadak-exif
uuid::Uuid::new_v4()
```

并且不依赖任何 host-only 模块（`publisher`、`source`、`storage`、`reviewer`、`runtime`、`cli`）。

### 1.2 重要警告：能编译 ≠ 能运行

实测结果：`std::fs`、`std::process`、`std::env`、`std::thread`、`SystemTime::now`
**都能在 `wasm32-unknown-unknown` 上通过编译**，但在 Worker 运行时会 panic 或返回错误。

因此：

```text
编译器无法强制 core 的纯净性
```

core 的纯净性只能靠：

* 代码审查
* 模块依赖方向（core 不得依赖 host）
* 以及"执行副作用的能力必须由 host 以端口形式注入"这一约束

这条结论直接决定了本拆分的基本方法：**数据与纯逻辑进 core，动词（副作用）留 host 并抽象成端口。**

---

## 2. 持久化事务边界

### 2.1 现状（实测，非设计目标）

```text
BEGIN IMMEDIATE / COMMIT   → 只出现在 schema 迁移里
每一个 store.save(...)      → 单条 INSERT OR IGNORE，没有显式事务
7 个 store                 → 7 个独立 SQLite 文件
```

七个状态文件（`.mineral/` 下；S6.2 增加 `delivery-projections.sqlite3`，
S6.3 增加 `asset-observations.sqlite3`）：

```text
document-reviews.sqlite3
asset-reviews.sqlite3
human-reviews.sqlite3
publish-runs.sqlite3
remote-observations.sqlite3
delivery-projections.sqlite3
asset-observations.sqlite3
```

### 2.2 结论：跨 store 原子性目前不存在

由于状态分散在 7 个数据库文件中，**没有任何一个事务能同时覆盖两个 store**。
所以系统当前依赖的不是原子性，而是**顺序 + 幂等**：

```text
1. 先写 publication intent（PublishRun）
2. 再执行副作用（CAS push）
3. 再写观察事实（RemoteObservation）
```

不变量是：

```text
intent 必须先于副作用
副作用之后必须有一次观察
```

恢复逻辑因此是"读 intent → 观察 → 对账"，而不是"回滚"。

如果将来需要真正的跨 store 原子性，只有两条路：合并到单一数据库文件，或引入显式的
journal/outbox。**当前未做，也不应该顺手做。**

S6.3 又新增了一类**不可回滚的外部对象**：published asset 放在 asset target 上
（native host 是本地目录，Cloudflare runtime 将是对象存储），它与 Git ref、
SQLite 三者之间同样没有事务。因此这里的原则不变：**顺序 + 幂等**，
content-addressed 的 staged orphan 允许存在，重试时重新 observe 而不是回滚。

S6.4 把这条路径的**运行时边界**从"完整内存 bytes"推进到 bounded streaming：
engine 仍然只持有 frozen facts 和自己的验证规则，host 侧 driver 逐块读取
immutable blob、边读边喂 `IncrementalBlobVerifier`，并且只有在验证通过之后才
`finish()` 提交写入。于是 20.92 MB 的 JPEG 不再需要被 materialize 成一个内存
buffer。真实对象存储由 host 的 R2 adapter（SigV4 自实现 + 临时 spool + 定长 PUT）
承担；**没有任何 R2 / HTTP / SDK 类型进入 mineral-core**，core 里的
`AssetTarget` 端口只看到 `ImmutableBlobSource`。

S6.4.1 把 delivery 的对象键从纯内容寻址扩展成
`assets/sha256/<prefix>/<hash>/<filename>`：`<hash>` 仍是 bytes identity，
`<filename>` 是 presentation identity（来自 logical path 的 basename，并与冻结的
`published_content_type` 对齐，避免 sanitizer 改变格式后后缀说谎）。同一个 hash 配不同
filename 会得到不同 object key，换掉物理去重换取 "URL path == object key"；public URL 只
对 filename segment 做 percent-encoding，R2 适配器始终只消费冻结的 `object_key`，
不理解 V1/V2。Durable 侧 `DeliveryProjectionWire` 现在写 V2、仍读 V1：V1 的历史
filename-less key 保持原样可恢复，并且 decode 后的 V1 会原样 re-encode，不做隐式升级。

S6.5 把 human decision 的复用键从"随机 review attempt id"改成
`(content_path, content_sha256, policy_identity)`。review attempt 每次失败重跑都会
生成新的随机 id，所以绑定 attempt 的决议永远追不上下一次尝试——provider 挂掉时
approve 之后再 publish 仍会再次调用 provider。现在：人工决议绑定被审查的内容 + policy，
review workflow 在调用 provider 之前先查该 subject 是否已有人工决议（是则直接复用
已有 attempt，**provider 调用为 0**），effective review set 也按 subject 查找。
`human-reviews.sqlite3` schema v1→v2 增加 identity 列；**历史行保持原样**（identity 列为
NULL，语义仍是"只对当时那一次 attempt 生效"，不迁移、不放大），新决议同时记录
identity 与引发它的 attempt。

### 2.3 已批准的 schema 变更

`publish-runs.sqlite3`：`user_version 1 → 2`

```sql
ALTER TABLE publish_runs ADD COLUMN publish_target_id TEXT NOT NULL DEFAULT '';
UPDATE publish_runs
   SET publish_target_id = remote_name || ':' || destination_ref
 WHERE publish_target_id = '';
PRAGMA user_version = 2;
```

* 回填值与 CLI 默认值同源（`{remote}:{reference}`），保证迁移前后的行互相匹配
* 空 target id 在读取时被拒绝（fail closed）
* 已在真实 `.mineral` 数据的**副本**上验证：版本升到 2、旧行获得派生身份、`status` 输出不变、二次打开幂等
* 真实 `.mineral` 目录未被改动，下次真实运行 CLI 时自动迁移

---

## 3. 内存与二进制解码

### 3.1 真实 vault 实测

```text
总大小              160.6 MB
最大单对象          20.92 MB JPEG
```

解码为 RGB 后的内存：

```text
3072 × 4096  →  36.0 MB
4928 × 6560  →  92.5 MB
```

### 3.2 关键事实：没有尺寸上限

```text
AssetProgramCheck  → 解码图片，无 size cap
AssetSanitizer     → 再次解码 + 重新编码
```

即：单张图片的处理峰值是**数倍于原始文件大小的解码缓冲区**，而且解码发生两次。

### 3.3 Cloudflare Worker 结论

```text
Workers isolate 内存上限  128 MB
单张 4928×6560 图片解码   92.5 MB
+ 重新编码缓冲
```

结论：

```text
Worker 无法运行当前的 AssetProgramCheck / AssetSanitizer 路径
```

这不是优化问题，而是平台边界问题。因此这两个执行器在拆分后**必须留在 host**，
由 core 通过端口调用（见 §4）。

### 3.4 wasm bundle 体积实测

```text
baseline                      0 KB
infer only                  573 KB
image + exif + infer       1630 KB
```

（仅作参考：体积不是当前决策依据，内存与能力才是。）

### 3.5 wasm 编译阻塞项

实测只有两个真实的**编译**阻塞：

```text
uuid v4          → wasm32 需要 js / rng-getrandom / rng-rand feature
reqwest::blocking → wasm32 上不存在
```

处理方式：ID 生成器与 reviewer 传输全部留在 host，core 不含任何 ID 生成实现。

以下依赖在 crate 级别实测 **WASM-OK**：
`base64`、`sha2`、`serde`、`serde_json`、`serde_yaml_ng`、`schemars`、`infer`、
`kamadak-exif`、`image`（6 种编码）、`rusqlite`(bundled)、`reqwest`(rustls)。

---

## 4. 端口与执行器的当前边界

### 4.1 已存在的端口

```text
Reviewer / ReviewRunStore
AssetReviewer / AssetReviewRunStore
HumanReviewStore
PublishRunStore / RemoteObservationStore
*IdGenerator ×4
BlobStore
GitRemote（observe_ref / observe_commit / compare_and_swap）
```

### 4.2 需要新增的端口（依据：core 侧 workflow 的实际调用）

| host 执行器 | 生产调用点（实测） | 结论 |
| --- | --- | --- |
| `AssetProgramCheck` | `AssetReviewWorkflow::execute_at` 内部构造（唯一生产调用点） | **必需端口** `AssetInspector` |
| `AssetSanitizer` | 只有 `PublicationApplication::run` | **应用层搬进 core 时才需要**，核心审核链不需要 |
| `GitCurrentTargetAdapter` / `GitProjectionMaterializer` / `GitCommitObjectCreator` | 发布准备序列 | **必需端口** `GitRepository` |

形状（core 侧，纯数据进出，执行器自己持有 blob 来源）：

```rust
pub trait AssetInspector {
    type Error: Error + 'static;
    fn inspect(
        &self,
        candidates: &CandidateAssetSet,
        snapshot: &Snapshot,
    ) -> Result<AssetCheckResult, Self::Error>;
}

pub trait AssetSanitizer {
    type Error: Error + 'static;
    fn sanitize(
        &self,
        effective_reviews: &EffectiveReviewSet,
        checks: &AssetCheckResult,
        snapshot: &Snapshot,
    ) -> Result<SanitizedAssetSet, Self::Error>;
}

pub trait GitRepository {
    type Error: Error + 'static;
    fn read_current(
        &self,
        base: &GitCommitOid,
        root: &ManagedRoot,
    ) -> Result<GitCurrentTarget, Self::Error>;
    /// 注意：端口不接受 blob store 参数。`BlobStore` 有关联 `Error` 类型，
    /// 用 `&dyn BlobStore` 会逼出 `dyn BlobStore<Error = ContentStoreError>`
    /// 这种写法，并限制其它 runtime 的实现。改为由 adapter 自己持有 blob 来源，
    /// 端口上只出现纯数据 —— 与 `AssetInspector` 的处理方式一致。
    fn materialize(
        &self,
        base: &GitCommitOid,
        projection: &PublicProjection,
    ) -> Result<ReviewedGitTree, Self::Error>;
    fn create_commit(&self, spec: &GitCommitSpec) -> Result<GitCommitOid, Self::Error>;
}
```

`read_current` 返回 `GitCurrentTarget`（而不是裸的 `CurrentTargetState`），因为引擎必须先核对
「本地解析出的 base commit == 远端观察到的 base」才能信任这份状态；只返回状态会丢掉这个不变量。

`create_commit` 只接受**已冻结的** `GitCommitSpec`，adapter 不得自己读时钟或决定身份；
`noop` 由引擎用 `reviewed.is_noop()` 判定后跳过调用，adapter 不参与策略判断。

### 4.4 混合文件的数据/执行器切分

`workflow/` 里三个文件同时含有纯数据与平台执行器，切分线如下：

| 文件 | 进 core（纯数据） | 留 host（执行器） |
| --- | --- | --- |
| `asset_program_check.rs` | `ActualAssetType`、`ImageDimensions`、`AssetCheckFinding`、`CheckedAsset`、`AssetCheckResult` | `AssetProgramCheck`、`check_bytes`、`check_image`、`detect_actual_type`、`AssetProgramCheckError` |
| `asset_sanitization.rs` | `SanitizedAsset`、`SanitizedAssetSet`、`ImageSanitizationFormat`、`SanitizationTransformation` | `AssetSanitizer`、`reencode`、`verify_*`、`AssetSanitizationError` |
| `asset_review_workflow.rs` | `AssetReviewWorkflowEntry`、`AssetReviewWorkflowResult`、`AssetReviewEvaluator`、`AssetReviewRunIdGenerator` 等纯部分 | `AssetReviewWorkflow::execute_at`、`AssetReviewWorkflowInput`、`AssetReviewWorkflowError/Failure` |

两条容易踩的线：

* `detect_actual_type` 调用 `infer::get`。把它搬进 core 就要给 core 加 `infer` 依赖，因此
  **类型检测留在 host**，host 负责填 `ActualAssetType`。
* `AssetReviewWorkflowFailure::ProgramCheck(AssetProgramCheckError)` 命名了 host 错误类型，
  所以 `Failure` 必须留 host —— 只有 `Result`/`Entry` 这些纯数据可以进 core。
  这也是 `effective_review_set` 等下游文件能否搬移的关键：**结果类型进 core，产生它的执行器留 host。**

### 4.3 `BlobStore` 的状态

```text
现状：同步 read(identity) -> Vec<u8> / store(bytes) -> identity
性质：host-era 临时接口，不是最终的 R2 async 接口
```

大对象（图片/视频）不应整体进入 wasm 内存。将来的方向是 `read_prefix(identity, max)`
与 adapter 之间直接流式复制（R2→CAS、CAS→Cloudflare Images），
让超大媒体永远不进入 wasm 线性内存。**当前不实现，但端口设计不得把它堵死。**

---

## 5. 状态

已完成并验证：

* §2.3 schema v2 迁移（在真实数据副本上验证）
* §3 全部测量数据
* §4.1 的 `GitRemote` + `publication/git` 模型（`GitRefTarget` / `GitCommitOid` / `GitTreeOid` /
  `GitCommitSpec` / `RefUpdate` / `CasOutcome` / `CommitObservation`）与 `domain::TimestampMillis`
* §4.4 的纯模型搬移：`mineral-core/src/workflow/` 12 个模块，host 只保留 4 个平台文件
* §4.2 的 `AssetInspector` 端口：`AssetReviewWorkflowInput` 用 `inspector: &C` 取代了
  `content_store: &B`，审核序列不再知道检查是怎么做的
* §4.2 的 `GitRepository` 端口与 host 适配器 `GitRepositoryAdapter<B>`（`read_current` /
  `materialize` / `create_commit`），`ReviewedGitTree` 与 `GitCurrentTarget` 已进 core，
  `GitCommitObjectCreator::create_from_spec` 提供纯 spec 入口* **§9 第 1 步（路线 A）已完成**：`PublishRun` / `PublishRunId` / `PublishRunPublication` /
  `PublishRunStore` / `PublishRunError` / `ReadyToPush` / `PublishReconciliation` /
  `RemoteRefObservation` / `RemoteObservationId` / `RemoteObservationStore` 已在
  `mineral-core/src/publish/`（`run.rs` / `reconciliation.rs` / `remote_observation.rs`）。
  `from_git_commit_result(&GitCommitResult, ...)` 由
  `from_reviewed_tree(..., desired_commit: Option<GitCommitOid>, ...)` 取代，
  `None` = Noop、`Some(oid)` = CommitReady；`PublishRunPublication` 保留为
  **派生只读视图**（由 `desired_commit` 计算，不可独立矛盾）。第 1 步本身不改 schema，
  只改 Rust 映射（`publication_kind` / `commit_oid` ↔ `Option<GitCommitOid>`）。
  纯 reconciliation/model 测试在 core；建立真实 canonical locator 的 reconciliation 测试
  迁到 host `conformance_publish_reconciliation.rs`；host 持久化实现留在 `mineral-host`。
* **§9 第 2 步已完成**：
  * `publication/git/wire.rs` 的 `CommitSpecWire`（`VERSION = 1`，字段名与 §6.6.4 一字不差，
    `deny_unknown_fields`）负责 `commit_spec` 的 encode/decode；未知版本 → `UnsupportedVersion`，
    结构错误 → `Malformed`，字段不可用 → `Invalid`。
  * `PublishRun` 增加 `commit_spec: Option<GitCommitSpec>` 与 `commit_spec()` 访问器；
    `from_parts` 在 load/validate-intent 阶段执行四态规则与 §7 交叉校验
    （`(None, Some)` → `CommitSpecWithoutDesiredCommit`；
    `parent` / `tree` / `author_time` / `committer_time` 与 run 不一致 → 各自 fail closed）。
    刻意**没有**用 `assert_eq!(desired.is_some(), spec.is_some())`。
  * SQLite `user_version 2 → 3`：纯加法 `ALTER TABLE publish_runs ADD COLUMN commit_spec TEXT`，
    旧行保持 NULL，不新增 CHECK（`ALTER TABLE` 表达不了，且会让迁移库与新建库行为不一致；
    非法组合由 core 在加载时统一拒绝）。新建库的 v3 schema 与迁移结果列布局一致。
  * 测试：core 4 条四态/交叉校验失败用例 + 4 条 wire 格式用例；
    host 5 条（frozen spec 重启存活、**真实 v2 库升级**、(None,Some) 落库后拒载、
    交叉不一致拒载、不可解码 commit_spec 拒载）。
* **§9 第 3 步已完成**：
  * `from_reviewed_tree(..., desired_commit, commit_spec, created_at)` 现在必须同时拿到两者：
    新 intent 的规则是 `(None, None)` / `(Some, Some)` 合法，`(Some, None)` → 新的
    `PublishRunError::DesiredCommitWithoutCommitSpec`，`(None, Some)` → `CommitSpecWithoutDesiredCommit`。
    **legacy 宽容只留在 `rehydrate`**：
    ```
    新建 from_reviewed_tree : (None,None) ✅  (Some,Some) ✅  (Some,None) ❌  (None,Some) ❌
    恢复 rehydrate          : (None,None) ✅  (Some,Some) ✅  (Some,None) ✅  (None,Some) ❌
    ```
    所以 `Some + None` 从这一步起只可能来自历史库加载；生产构造点已经无法制造
    LegacyNonReconstructible。
  * `git_publication.rs` 只冻结**一次** spec，同一个值既交给
    `GitCommitObjectCreator::create(&spec)`，也交给 `PublishRun::from_reviewed_tree(..., Some(spec))`；
    Noop 走 `(None, None)`。没有"读两次 config 造两份 spec"的路径。
  * 冻结时刻统一：`created_at` = 配置里的固定 timestamp（若配置了）否则本次 attempt 的唯一
    clock reading；commit object 与 intent 共用它，因此 §7 的
    `spec.author_time == spec.committer_time == run.created_at` 由构造保证。
    CLI 走 `GitCommitMetadata::new(...)`（无固定 timestamp），生产行为不变。
  * 测试：core +4（新建两合法态、`(Some,None)` 拒绝、`(None,Some)` 拒绝、
    "同一 `(Some,None)` 输入在 rehydrate 合法而在新建非法"的边界对照）；
    host +2（真实发布后从库里取回的 spec 能单独 `create_from_spec` 重建出
    `desired_commit`；Noop 发布持久化 `(None, None)` 且远端与本地 HEAD 不变）。

* **§9 第 4 步已完成**：prepare 阶段已搬进 core
  （`mineral-core/src/publication/git/prepare.rs`），`GitRepository` 第一次成为生产消费者。
  * `GitPublicationPreparer::prepare(&R: GitRepository, &GitPublicationPrepareRequest)`
    完成整个 prepare 序列并返回 `GitPublicationPreparation`
    （`publish_run()` + `publish_plan_sha256()`）；**不落库、不碰远端、不做 reconciliation**。
  * 序列：`read_current` → 解析并核对 base → `PublishPlan::build` → `materialize` →
    核对 reviewed tree 属于该 base → 核对 plan/tree 的 noop 一致性 → 判定 Noop →
    冻结 `GitCommitSpec` → `create_commit(spec)` → 构造 `PublishRun`。
  * core 侧 fail-closed 校验：`ResolvedBaseInvalid` / `ResolvedBaseMismatch` /
    `MaterializedBaseMismatch` / `PlanReviewedTreeMismatch` / `ReviewedTreeInvalid` /
    `CommitSpec`；Noop **不调用 `create_commit`**，`create_commit` 返回的 OID 就是
    `desired_commit`，交给它的 spec 就是被持久化的 spec。
  * 深度校验的分工：`create_commit` 之后对 object DB 的读回校验（tree/parent 是否真的是
    刚创建的那个对象）只有 runtime 能做（`GitCommitObjectCreator::create_from_spec` 已经在做），
    引擎侧由 `PublishRun` 的 §7 交叉校验兜底；端口契约本身不返回可读回的 commit 事实。
  * host 侧 `git_publication.rs` 退化为 composition wrapper：canonical repository identity →
    远端观察（`GitRemoteObserver`，**仍留在 host**）→ 构造 `GitRepositoryAdapter` →
    冻结 attempt instant → 调 `GitPublicationPreparer::prepare` → `save` →
    旧执行路径 `PublicationWorkflow::execute`。三个静态入口
    （`GitCurrentTargetAdapter` / `GitProjectionMaterializer` / `GitCommitObjectCreator`）
    不再出现在 prepare 生产路径上。
  * 一处配套：core 的 `BlobStore` 增加了 `impl<B: BlobStore> BlobStore for &B`
    （`GitRepositoryAdapter` 按值持有 blob source，组合根只能借出）。
  * 测试：core +10 纯 fake-port 用例（真实改动 → create_commit 恰好 1 次且 spec 与使用的
    完全一致、Noop → create_commit 0 次、base 不一致、base 不可用、reviewed tree 属于另一个
    base、plan 与 tree 冲突、read_current 失败、materialize 失败、commit identity 不可用、
    create_commit 失败 → 不产生 intent）；host 的真实 Git 测试保持原样，现在成为
    穿过 `GitRepository` 端口的 conformance 测试。

* **§9 第 5 步已完成**：执行序列已搬进 core
  （`mineral-core/src/publication/git/execute.rs`），`GitPublicationExecutor` 同时依赖
  `GitRepository`（本地 object 事实）与 `GitRemote`（远端 ref 事实）。
  * 序列：reload intent → `observe_ref` → 持久化 observation → `PublishReconciliation`
    → （仅在需要 commit 且远端正好是 expected base 时）验证/恢复 desired commit →
    显式 `RefUpdate{expected_old,new_commit}` 的 exact CAS → 再次 `observe_ref` →
    持久化 observation → 再次 reconcile → 分类。**CAS 返回 `Updated` 不等于 `Published`**：
    必须以 CAS 之后那次 observation 的 reconcile 结果为准。
  * `GitRepository` 新增一个只报事实的窄能力
    `inspect_commit(&GitCommitOid) -> LocalCommitState{Missing, Present(GitCommitFacts{commit,parent,tree})}`。
    本地 object 检查因此留在 repository 端口上，**没有塞进 `GitRemote`**；
    parent/tree 是否可信仍由 core Executor 判（`CommitIdentityMismatch` /
    `CommitParentMismatch` / `CommitTreeMismatch` 全部 fail closed，且都发生在 CAS 之前）。
  * 恢复语义：commit 缺失 + `(Some, None)` → `LegacyCommitNotReconstructible`，
    **绝不读当前 config 补 spec**；commit 缺失 + `(Some, Some)` → `create_commit(run.commit_spec)`，
    重建出的 OID 必须 `== desired_commit`（否则 `ReconstructedCommitMismatch`），
    再读回一次校验 parent/tree。
  * 新增 `ports::Clock`（`now() -> Option<TimestampMillis>`）+ host `SystemClock`：
    两次 observation 各自读取一次时钟，engine 自身不读钟。
    `RemoteObservationIdGenerator` 从 host 移入 core（纯端口），host 保留
    `Sequential…` / `Uuid…` 适配器。
  * `PublicationWorkflowResult` → core `GitPublicationExecution`（host 保留同名 type alias）；
    host `PublicationWorkflow::execute` 变成纯绑定壳（7 参数：store ×2 + id 生成器 +
    `GitRepository` + `GitRemote` + `Clock`），序列本身不再在 host 里。
    host 的 `GitCompareAndPushExecutor` / `GitPushExecutionResult` 暂时保留，但已不再有生产消费者。
  * 测试：core +20 纯 fake-port 用例，含 §9 硬要求项：unknown `PublishRunId` 时
    `observe_ref`/`CAS`/`inspect_commit` 调用数均为 0；初始远端已是 desired → CAS 0 次；
    远端不在 expected base → CAS 0 次；desired commit 的 tree/parent 不符 → CAS 0 次；
    legacy 缺失 → 不 create/不 CAS；重建 OID 不符 → CAS 0 次；CAS 被拒 → lost race 而非执行失败；
    CAS 成功但远端未动 → 不判 Published；post observation 失败/落库失败 → 语义明确的
    `Indeterminate` 或 typed error；**崩溃重试幂等**（CAS 已成功但 post observation 未能落库 →
    重试 reload intent → 初始 observation 看到 desired → `AlreadyPublished`，第二次 CAS = 0）。
    host 的真实 Git 用例现在穿过 core Executor，成为端到端 conformance 测试。

* **§9 第 6 步已完成**：S5 只剩一条 publication path。
  * 删除：`GitCompareAndPushExecutor` / `GitPushExecutionResult` / `GitPushExecutionError`
    （整个 `publisher/git_push_executor.rs`）、`GitCommitResult` / `GitCommitNoop` /
    `ReviewedGitCommit` / `GitCommitObjectCreator::create` / `GitCommitMetadata::to_spec`、
    `GitRemote::observe_commit`（+ 它专用的 core `CommitObservation`）、host
    `publication_workflow.rs`（`PublicationWorkflow` 纯绑定壳、`PublicationWorkflowResult`
    type alias）。
  * 保留并明确职责：`GitCommitObjectCreator::{create_from_spec, inspect}`、
    `GitRemoteAdapter`、`GitRepositoryAdapter`、SQLite stores、`SystemClock`、
    `Sequential…`/`Uuid…` ID 分配器（`SequentialRemoteObservationIdGenerator` 移到
    `git_publication.rs` 与其余分配器同处）。
  * `GitPublicationApplication::{prepare_and_publish, resume}` 直接调用
    core `GitPublicationExecutor::execute`；`resume` 保留 **plumbing read**，
    注释明确它只用于绑定 runtime adapter，权威 reload 仍由 Executor 在任何远端副作用之前完成。
  * `GitRemote` 端口现在只有两个方法：`observe_ref` / `compare_and_swap`。
    `CasOutcome` 保持 `{ Updated, Rejected }`（`Rejected` 不携带远端状态，避免第二事实源）。
  * 旧 push executor 的真实 Git 用例里，属于 **adapter 层**的那 5 条没丢：迁到
    `crates/mineral-host/src/conformance_git_remote.rs`（exact CAS 移动 ref 并被新 observation 确认、
    并发前进被 exact lease 拒绝、远端 rewind 即使可 fast-forward 也被拒绝、
    stale remote-tracking ref 不控制 lease、CAS 不改 worktree/index/HEAD/本地分支）。
    属于旧 orchestration 的其余用例由 core 的 20 条 fake-port 用例与 host 端到端用例覆盖。

### 5.1 冻结点与剩余工作（S5 完成后的状态）

**已冻结的状态**：publication 只有一条路径（`GitPublicationPreparer` 创建 / `GitPublicationExecutor` 恢复），
`GitRepository` / `GitRemote` / 两个 Store / `Clock` / ID 生成器均为 core 端口，host 只剩适配器与组合根。
六项绿灯：fmt / check / clippy -D warnings / 全量测试 / core wasm32 / `git diff --check`。

以下按**建议执行顺序**列出剩余工作。R1、R2 是纯机械且语义为零的清理，适合各自独立提交；
R3 是需要设计的真实架构洞；R4、R5 是拆分收尾。

#### R1. 揭掉 core/host 兼容层（纯搬迁残留，语义为零）

* `crates/mineral-host/src/lib.rs:8`：`pub use mineral_core::{content, domain, policy, ports, publication, publish};`
* `crates/mineral-host/src/workflow/mod.rs:12-35`：把 core `workflow` 的约 70 个条目 re-export 进 host 的**同名**
  模块，于是 `crate::workflow::` 同时指向两个 crate 的东西（这是唯一"同名双源"的模块）。
* 规模（冻结时实测）：host 内通过兼容层解析的引用 **93 处 / 29 个文件**，
  另有 11 处 `mineral_publisher::`（4 个 examples + `cli.rs` + `lib.rs`）；
  已经写成 `mineral_core::` 的只有 9 处。
* **必须按条目判断归属，不能盲替换**：`crate::workflow::PublicProjection` → core，
  而 `crate::workflow::PublicationApplication` 必须留在 host。
* **绝对不要改**两组 hash domain 字面量——它们是持久化身份的一部分，改了会让
  `projection_sha256` / `plan_sha256` 与历史数据、reviewer contract 哈希全部失配：
  * `mineral-core/src/workflow/public_projection.rs:249` `b"mineral-publisher-public-projection-v1\0"`
  * `mineral-core/src/workflow/publish_plan.rs:241` `b"mineral-publisher-publish-plan-v2\0"`
  * （测试临时目录前缀里的 `mineral-publisher-…` 纯属命名，留或改都无副作用。）
* 验收：同样的测试数、六项绿灯、0 diagnostics，且 core→host 仍为 0。
* 注意：兼容层**不影响 core 纯度**。core 的隔离由 Cargo 依赖方向保证
  （`mineral-core` 不依赖 `mineral-publisher`）；它只影响 host 侧的可读性。

#### R2. package 名与二进制命名对齐（元数据，独立提交）

* `crates/mineral-host/Cargo.toml:2`：`name = "mineral-publisher"` → `mineral-host`，
  保留 `[[bin]] name = "mineral"`、`path = "src/main.rs"`。
* 同步 `docs/architecture.md`（第 42 节部署图等）与任何脚本 / `cargo -p` 调用。
* 顺带修正一处**用户可见**的既有不一致：`crates/mineral-host/src/cli.rs:369` 打印
  `mineral-publisher review list`，而实际二进制名是 `mineral`，应为 `mineral review list`。

#### R3. Projection persistence（真实架构洞，需要设计，单独开一个步骤）

* 目标：补上 §6.6.1 的最终恢复语义 —— object DB 全灭时
  `run 的 Projection 身份 → ProjectionStore → immutable BlobStore → rematerialize →
   重新生成的 tree OID 必须 == run.reviewed_tree → create_commit(run.commit_spec) →
   重建出的 commit OID 必须 == run.desired_commit`。
* 需要：`ProjectionStore` 端口 + Projection 持久化 + `PublishRun` 绑定一个可定位的 immutable Projection。
  会涉及 **schema 变更**（新表或新列）与 **prepare 阶段写库**（今日 prepare 明确不落库，只能由调用方 save）。
* 现状与降级：`PublishRun` 只有 `snapshot_id` / `projection_sha256` / `managed_root`，
  没有任何可加载 immutable Projection 的端口，也没有把 Projection 持久化的表；
  第 5 步在该分支以 `CommitCreation(runtime error)` fail closed（正确降级，但能力缺失）。
* 明确禁止：不得用"当前 Projection"或当前 config 重算这条路径。
* 恢复能力现状：持久本地 Git repo → 基本完整；commit object 丢失但 tree 还在 → 可重建；
  **整个 object DB 丢失 → 目前只能 fail closed**；Cloudflare ephemeral Container → 依赖本项。

#### R4. `PublicationApplication` 搬 core

* 现在仍在 host：`crates/mineral-host/src/workflow/publication_application.rs`，
  仍接受 `&Path` / `GitCommitMetadata`。
* 搬移时需要把 host 的 `workflow/asset_sanitization.rs` 抽成 **`AssetSanitizer` 端口**。

#### R5. CLI 拆到 `apps/mineral-cli`

* 现在仍在 `crates/mineral-host/src/cli.rs`（约 850 行），与 host 库同 crate；
  拆分后 host 只保留适配器与组合根，CLI 成为独立 workspace member。

本文档的模块清单会随 S5/S6 的搬移继续更新。

---

## 6. S5 完成条件（动代码前已钉死）

### 6.1 Prepare / Execute 的唯一契约

```rust
let run = preparer.prepare(...)?;   // 不落库
publish_run_store.save(&run)?;      // 持久化断点（唯一）
executor.execute(run.id(), ...)?;   // 只接受 ID
```

* `GitPublicationExecutor::execute` **只接受 `PublishRunId`**，第一件事必须是从
  `PublishRunStore` 重新加载 immutable `PublishRun`；加载不到即 fail closed，
  **在任何远端副作用之前**返回错误。
* 不引入 `PersistedPublishRunId` typestate：**store reload 本身就是持久化证明**。
  调用方即使误传未落库的 ID，结果也是"0 次远端副作用"。
* `create_commit` 只写本地 object database，不算远端副作用，因此可以发生在 intent 之前。

### 6.2 commit identity 必须可在恢复时重建

**结论：采用 B（持久化冻结后的 `GitCommitSpec`），而不是 A（从零散字段重建）。**

理由（实测字段来源）：

| `GitCommitSpec` 输入 | 现状来源 | 是否已在 `PublishRun` |
| --- | --- | --- |
| parent | 远端观察到的 base | ✅ `base_commit` |
| tree | materialize 结果 | ✅ `reviewed_tree` |
| author/committer 时间 | 冻结的 attempt time | ✅ `created_at_unix_ms`（两者同值） |
| **author/committer 名与邮箱** | `mineral.yaml` 的 `git.author_*` | ❌ **可变 runtime config** |
| **message** | `mineral.yaml` 的 `git.message` | ❌ **可变 runtime config** |

因此 A 不可行：重建必须去读当前配置，而配置可能已经改了
（第一天 `message = "Mineral publish"`，第二天改成 `"Automated publish"`，
同一个 PublishRun 就再也重建不出同一个 commit）。所以：

> **影响 `desired_commit` identity 的所有输入，必须在 `PublishRun` intent 持久化时已经冻结，
> 且恢复过程不得读取任何 mutable runtime configuration。**

落地形状：

```rust
pub struct PublishRun {
    // ...
    reviewed_tree: GitTreeOid,
    desired_commit: Option<GitCommitOid>,   // None = Noop
    commit_spec: Option<GitCommitSpec>,     // None = Noop；与 desired_commit 同生同灭
}
```

恢复时若 `observe_commit(desired_commit)` 报 `Missing`（ephemeral 环境、object database 丢失）：

```text
create_commit(run.commit_spec)
      ↓
recreated OID 必须 == run.desired_commit
      ↓
否则 fail closed
```

持久化影响：`publish-runs.sqlite3` 需要一次**加法式**迁移（`user_version 2 → 3`），
新增可空的 `commit_spec` 列；`desired_commit` 复用已有的 `publication_kind` / `commit_oid`。
**历史行（迁移前写入）的 `commit_spec` 为 NULL**，语义是"无法重建，必须本地存在该 object" ——
这是对既有数据的诚实降级，新行一律携带 spec。

### 6.3 Noop 验收（1 PublishRun = 0 commit）

```text
reviewed.is_noop() == true
  → create_commit 调用次数 = 0
  → desired_commit = None, commit_spec = None
  → PublishRun 仍然落库
  → CAS 调用次数 = 0
```

### 6.4 边界验收（grep 可验证）

core 的 publication workflow 内不得出现：

```text
GitCurrentTargetAdapter::
GitProjectionMaterializer::
GitCommitObjectCreator::
GitCompareAndPushExecutor::
```

只能出现 `GitRepository` / `GitRemote` / `PublishRunStore` / `RemoteObservationStore`。

**§9 第 6 步后的验收结果（全部为 0 命中）**：

```text
core：GitCurrentTargetAdapter / GitProjectionMaterializer / GitCommitObjectCreator /
      GitRemoteAdapter / GitPublicationApplication / rusqlite / std::process /
      mineral_host / crate::publisher
host 生产代码：GitCompareAndPushExecutor / GitPushExecutionResult / GitPushExecutionError /
      GitCommitResult / GitCommitNoop / ReviewedGitCommit / PublicationWorkflow /
      GitRemote::observe_commit / GitCommitMetadata::to_spec
业务调用点唯一性：
      compare_and_swap(  → 只有 core GitPublicationExecutor（端口定义与 adapter 实现不算）
      create_commit(     → 只有 core GitPublicationPreparer（正常创建）
                           与 core GitPublicationExecutor（恢复重建）
```

也就是说：正常创建 commit 只有 Preparer 一条路，恢复重建只有 Executor 一条路，
不存在第三条 publication 业务入口。

### 6.5 测试分层

```text
core tests            → 给纯事实，验证 reconciliation 算法
host conformance tests → 制造真实 Git 状态，验证 adapter 观察出的事实与真实 Git 行为一致
```

### 6.6 恢复语义的完整条件（ephemeral Git runtime）

#### 6.6.1 spec 能重建 commit，但前提是 tree object 存在

`create_commit(spec)` 不能凭空造出 `tree = abc123`：那棵 tree 必须已在 object DB 里。
所以本地 Git object database 整体消失时（未来的 Worker → 临时 Container 场景），完整恢复路径是：

```text
load persisted PublishRun
        ↓
desired commit 存在？ ── yes → verify
        │
        no
        ↓
reviewed tree 存在？ ── yes
        │
        no
        ↓
load persisted Projection（PublishRun 必须能定位它）
        ↓
immutable BlobStore
        ↓
GitRepository.materialize(...)
        ↓
重新生成的 tree OID 必须 == run.reviewed_tree
        ↓
create_commit(run.commit_spec)
        ↓
重建出的 commit OID 必须 == run.desired_commit
```

不满足任一相等条件即 fail closed。这条把恢复能力从"长期存在的本地仓库"
扩展到"临时执行环境"。

#### 6.6.2 `desired_commit` / `commit_spec` 有四种组合，三种合法

| `desired_commit` | `commit_spec` | 含义 |
| --- | --- | --- |
| `None` | `None` | Noop |
| `Some` | `Some` | 新格式，可重建 |
| `Some` | `None` | 历史格式（迁移前写入），只能依赖现存 Git object |
| `None` | `Some` | **永远非法** |

**不得**用 `assert_eq!(desired_commit.is_some(), commit_spec.is_some())` 校验 ——
那会把迁移后的历史数据整体判为 corrupt。core 侧要显式 match 四种组合并各写测试。
暂不引入 `enum DesiredGitState`；若将来到处都在 match 这两个 `Option`，再升级。

#### 6.6.3 commit identity 包含 timezone offset

Git commit object 里 author/committer 行不只存 Unix 秒，还带时区偏移。已核对：

```rust
// git_commit_object.rs:467
fn git_date(time: TimestampMillis) -> String {
    format!("@{} +0000", time.as_unix_seconds())   // 固定 +0000，不取机器时区
}
```

即 offset 由 adapter 固定为 UTC，跨机器确定性成立。待办：加一条单元测试**钉住**这个格式，
防止将来有人改成读取本地时区而使「同 spec → 同 OID」静默失效。
更强的做法是让 `GitCommitSpec` 自己冻结 offset（当前不需要，因为它是常量）。

#### 6.6.4 `commit_spec` 的持久化格式必须版本化

数据库列仍然只有一个可空 TEXT：

```sql
commit_spec TEXT NULL
```

但内容从第一天起就带版本号：

```json
{
  "version": 1,
  "parent": "...",
  "tree": "...",
  "author":    { "name": "...", "email": "...", "time_unix_ms": 0 },
  "committer": { "name": "...", "email": "...", "time_unix_ms": 0 },
  "message": "..."
}
```

避免将来 `GitCommitSpec` 结构变化后，旧 PublishRun 被
`serde_json::from_str::<CurrentGitCommitSpec>()` 悄悄绑死。
这个系统强调 `commit → PublishRun → Projection → Review → Snapshot` 的长期可审计性，
所以 durable wire format 从第一天就版本化。

#### 6.6.5 orphan local object 是无害的（设计事实）

```text
materialize → create_commit 成功 → save PublishRun 失败/crash
```

结果是：

```text
本地 orphan Git commit object   存在
远端副作用                        0
durable PublishRun                0
```

这是可接受的：**在 intent 持久化之前创建的本地 Git object 不构成 publication**，
可以之后被 GC。明确写出这条，是为了避免将来有人为了"绝不产生 orphan object"
把 commit 创建挪到持久化之后，反而破坏 §6.1 的恢复模型。

#### 6.6.6 S5 完成条件总表

```text
1. Executor 只接受 PublishRunId
2. 第一件事是从 PublishRunStore reload
3. reload 失败 → 零远端副作用（用 CountingGitRemote 断言
   observe_ref / observe_commit / CAS 调用次数均为 0，而不是只判断返回错误）
4. 新的非 Noop run 冻结带版本的 GitCommitSpec
5. desired_commit / commit_spec 支持 Noop / Reconstructible /
   LegacyNonReconstructible 三态，并拒绝 None+Some
6. commit identity 的输入包含确定性的 timezone 语义（当前固定 +0000，需测试钉住）
7. 本地 tree object 丢失时：persisted Projection + immutable blobs
   → 重新 materialize → tree OID 必须 == reviewed_tree
8. 重建的 commit OID 必须 == desired_commit
9. 以上全部校验通过之后，才允许执行 exact CAS
```

---

## 7. 加载 intent 时的交叉校验（搬 `PublishRun` 时一并落实）

反序列化成功 **不等于** 事实一致。core 在加载 `PublishRun` 时必须自己核对
`commit_spec` 与 `PublishRun` 自身字段，矛盾在 **load/validate intent** 阶段就 fail closed，
而不是留给后面某道校验偶然挡住。

新格式的非 Noop run 必须同时满足：

```text
desired_commit              = Some(...)
commit_spec                 = Some(...)
commit_spec.parent          == publish_run.base_commit
commit_spec.tree            == publish_run.reviewed_tree
commit_spec.author_time     == publish_run.created_at
commit_spec.committer_time  == publish_run.created_at
```

前两条尤其重要：数据库损坏、旧版本 bug 或人工修改都可能制造

```text
PublishRun.reviewed_tree = TREE_A
commit_spec.tree         = TREE_B
```

若只信任 JSON，恢复逻辑会拿 `TREE_B` 成功重建出一个 commit，直到更晚才可能被发现。

历史形态 `Some(desired_commit) + None` 只允许一种动作：

```text
verify existing object
```

**不得**尝试从当前 runtime config "补" 一个 spec —— 那正是 §6.2 禁止的路径。

## 8. v2 → v3 migration 的验收测试（必须打真实旧库）

不要只测"新建一个 v3 DB"。本次迁移最需要保护的路径是**真实旧库升级**：

```text
创建真实 v2 DB
→ 插入两类历史行
     · Noop 行
     · 有 commit 的历史行
→ 执行 v3 migration
→ reopen
→ 历史行 commit_spec == NULL
→ (Some, None) 被识别为 LegacyNonReconstructible（而不是 corrupt）
→ 其余字段逐项不变
```

## 9. 提交粒度（每步可独立回滚）

执行纪律（每一步都必须满足）：

```text
1. 每一步只做一个语义变化
2. 每一步都保留同一组五项绿灯：
   fmt / check / clippy -D warnings / 全量测试 / core wasm32 / git diff --check
3. 每个新 invariant 至少配一个失败用例，不能只测 happy path
```

其中四类失败用例是硬要求，缺一不可：

```text
· LegacyNonReconstructible  → (Some, None) 被接受为历史格式，且只能 verify 现存 object
· None/Some Invalid         → (None, Some) 被判定为非法并 fail closed
· cross-field mismatch      → commit_spec.parent / tree / times 与 PublishRun 不一致时 fail closed
· unknown PublishRunId      → Executor 对不存在的 run 返回错误，且 observe_ref /
                              observe_commit / CAS 调用次数均为 0（用计数 fake 断言）
```

```text
1. 纯模型搬迁：PublishRun / ReadyToPush / PublishReconciliation /
   RemoteRefObservation + 两个 Store port
   —— 只做 ownership / module relocation，不动 publication sequence
2. v2 → v3 migration：nullable commit_spec TEXT，version: 1 的 encode/decode，
   旧行保持 NULL；四态校验 + §7 交叉校验 +
   §8 的真实旧库升级测试
3. from_reviewed_tree：Noop → (None, None)；普通 run →
   (Some desired_commit, Some frozen spec)，确保 runtime config 到此已完全冻结
4. Preparer 搬 core：GitRepository 第一次成为真实 consumer，
   带上 base / tree / spec / desired_commit 的全部 fail-closed 校验
5. Executor 搬 core：只收 PublishRunId，第一步 reload；
   随后 observation → recovery → verification → exact CAS →
   observation → persistence → reconciliation
6. Host 老入口降级为 wrapper，并执行 §6.4 的全局 grep 验收
```

### 9.1 第 1 步的排序约束（执行前必读）

> **状态：第 1 步已完成，采用路线 A。** 落地结果见 §5；下面保留当时的排序约束与
> 两路线对比作为决策记录。

第 1 步里**每一个类型都依赖 `PublishRun`**：`RemoteRefObservation` 绑定 `PublishRunId` 与
`PublishRun.target()`；`ReadyToPush` / `PublishReconciliation` 持有 `PublishRunId` 并读取
`publish_run.base_commit()` / `publication()`；两个 Store port 也引用它们。
所以第 1 步没有"可以单独搬一个类型"的子切片，**`PublishRun` 必须第一个搬，且必须与它的
构造器替换同一步完成**。

`PublishRun` 现在有两个 host 依赖需要同时解决：

```text
from_git_commit_result(&GitCommitResult, created_at)   ← GitCommitResult 在 host
PublishRunPublication::CommitReady { commit_oid: String }
```

两条可行路线（选一条，不要混）：

```text
路线 A（推荐，符合 §6.2）：
  · 用 from_reviewed_tree(id, target_id, repository, target,
      &ReviewedGitTree, desired_commit: Option<GitCommitOid>, created_at) 取代它
  · PublishRunPublication::CommitReady { commit_oid: String } 随之改成
      desired_commit: Option<GitCommitOid>（Noop 由 None 表达）
  · 代价：SQLite store 的 publication_kind/commit_oid 读写 + 约 6 处测试构造点要改
  · 好处：一步到位达到 §6.2 的领域类型要求，不再有 String 形态的 commit 身份

路线 B（更小语义变化，但把 host 数据搬进 core）：
  · 把 GitCommitResult / GitCommitNoop / ReviewedGitCommit 这三个纯数据类型也搬进 core
  · from_git_commit_result 原样保留
  · 代价：这三个类型目前由 host 的 GitCommitObjectCreator 用私有字面量构造，
    需要补 runtime 侧构造器；且 §6.2 的 desired_commit 改造要留到第 3 步
```

无论哪条路线，`commit_spec` 字段与 SQLite 列都在**第 2 步**才加入，第 1 步不动 schema。

