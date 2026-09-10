# Mineral Publisher Agent 开发约束

本文件用于约束参与 Mineral Publisher 开发的 AI Agent。

Agent 在修改项目之前，应优先阅读：

* `AGENTS.md`
* `docs/architecture.md`

本文件主要约束系统行为、架构边界和开发方式。

除非任务明确要求，否则不要因为本文件而预先锁定具体：

* 编程语言
* Web 框架
* 数据库
* ORM
* AI SDK
* Markdown 解析库
* Git 库
* 前端框架
* 部署技术

具体技术方案应根据当前任务、现有代码、成熟度、维护成本和部署目标决定。

---

# 1. 项目目标

Mineral Publisher 是一个自动化、可审核、可追溯的内容发布服务。

核心流程：

```text
Source
  ↓
Snapshot
  ↓
ChangeSet
  ↓
Policy
  ↓
Projection
  ↓
PublishPlan
  ↓
Publisher
```

公开发布流程通常还包括：

```text
Private Filter
  ↓
Program Check
  ↓
AI Review
  ↓
Asset Resolution
  ↓
Asset Review
```

---

# 2. 核心架构原则

以下规则属于项目核心不变量。

除非明确修改系统设计，否则实现不得违反这些规则。

## 2.1 Markdown 是最小发布单元

一篇 Markdown：

```text
要么整篇发布
要么整篇不发布
```

第一版不假设存在 Markdown 内部分块发布机制。

---

## 2.2 静态资源是 Markdown 的依赖

图片、PDF、音频、视频以及其他二进制附件不是独立发布主体。

资源只有在被最终发布的 Markdown 依赖时，才允许进入对应 Projection。

必须满足：

```text
No document, no asset.
```

最终公开结果中不得存在孤立资源。

---

## 2.3 Markdown 链接不能传播发布权限

例如：

```text
public.md
  ↓
[[private.md]]
```

不能因此自动发布 `private.md`。

Markdown 链接与静态资源依赖必须区别处理。

---

## 2.4 Public Policy 默认允许

当前公开策略采用：

```text
默认允许
↓
命中明确 private 规则
↓
排除
```

明确 private 规则优先于默认公开行为。

AI 不允许推翻明确的 private 决策。

---

## 2.5 Private 内容不得进入外部 AI

私有过滤必须发生在任何外部 AI 请求之前。

被确定为 private 的内容不得发送给外部 AI Provider。

这是安全边界，而不是普通业务建议。

---

## 2.6 AI 没有直接发布权限

AI Reviewer 只负责返回审核结果，例如：

```text
Pass
NeedsReview
Block
```

AI 不应直接：

* 修改 Source
* 修改历史 Snapshot
* 提交 Git
* Push
* 部署网站
* 获得不必要的发布凭据

审核能力和发布能力必须隔离。

---

## 2.7 AI 失败必须 Fail Closed

公开发布中出现：

* AI 不可用
* 请求超时
* 返回格式错误
* 结构化结果无法解析
* 审核过程异常

不得因此默认通过。

应该进入：

```text
NeedsHumanReview
```

或明确失败状态。

---

## 2.8 Snapshot 不可变

Snapshot 创建后不得修改。

源发生变化时，应创建新的 Snapshot。

---

## 2.9 ChangeSet 与 Projection 必须区分

```text
ChangeSet
=
发生了什么变化
```

```text
Projection
=
目标当前应该是什么状态
```

最终发布状态应由完整 Projection 决定，而不是仅依赖历史 Diff 不断 Patch。

---

## 2.10 Policy、Projection、PublishPlan、Publisher 解耦

Policy：

```text
决定什么允许发布
```

Projection：

```text
描述目标最终应该包含什么
```

PublishPlan：

```text
描述这一轮需要发生什么变化
```

Publisher：

```text
负责把变化应用到具体目标
```

不要把某种 Policy 与某一种 Publisher 硬编码绑定。

---

## 2.11 Markdown 的必需资源必须全部满足发布条件

如果 Markdown 的必要资源没有通过审核，则 Markdown 本身不能完成发布。

例如：

```text
article.md   PASS
image-a.jpg  PASS
image-b.jpg  BLOCK
```

结果应为：

```text
article.md -> BlockedByAsset
```

不得产生 Markdown 已发布但必要资源缺失的结果。

---

## 2.12 Projection 中不得存在孤立资源

最终资源集合应从最终 Markdown 集合重新计算。

概念上：

```text
FinalAssetSet = dependencies(FinalMarkdownSet)
```

任何资源都必须至少被一个最终 Markdown 使用。

---

## 2.13 Git 发布具有明确的 Commit 语义

如果目标 Publisher 使用 Git：

```text
1 PublishRun = 0 或 1 个 Commit
```

目标状态没有变化时：

```text
Noop
```

有实际变化时，应形成一个明确的 Commit。

---

## 2.14 审核内容必须等于最终提交内容

对于需要审核后提交的 Git 发布：

```text
Reviewed Tree
=
Committed Tree
```

如果审核完成后待提交内容发生变化，应终止原发布流程并重新验证。

实现方式可以根据 Git 技术方案决定，但这个不变量不能丢失。

---

## 2.15 禁止破坏发布历史

发布器不得通过强制覆盖历史的方式静默解决并发冲突。

例如 Git Publisher 不应通过 Force Push 覆盖未知远端变化。

冲突应该被检测并显式处理。

---

## 2.16 Secret 不得进入非必要输出

API Key、Token、Password、Credential 等 Secret 不得写入：

* 发布内容
* Git 历史
* 普通日志
* Audit
* 不必要的持久化数据

---

# 3. 工程原则

## 3.1 优先简单实现

不要因为未来可能需要而提前加入：

* 微服务
* 分布式执行
* 消息队列
* 通用插件体系
* 通用规则 DSL
* 复杂权限系统
* 大型前端架构

只有真实需求出现时才增加复杂度。

---

## 3.2 不提前实现未要求功能

如果任务只要求实现一个子系统，不要主动扩展到无关子系统。

例如实现 Snapshot 时，不应该顺便完整实现：

```text
R2
AI
Git Publisher
Web UI
```

---

## 3.3 不无理由增加依赖

新增外部依赖前，应判断：

* 当前任务是否真正需要
* 是否已有更简单实现
* 项目现有技术是否已经解决
* 该依赖是否成熟、维护活跃
* 引入后是否增加明显部署负
