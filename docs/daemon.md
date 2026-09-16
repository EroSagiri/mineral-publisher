# 调度与常驻控制台

构建并启动：

```sh
cargo build --release -p mineral-publisher
npm --prefix web-ui ci
npm --prefix web-ui run build
export MINERAL_WEB_TOKEN="$(openssl rand -hex 32)"
# 将此凭据保存在密码管理器中；浏览器登录时输入相同值。
./target/release/mineral --config mineral.toml daemon --assets web-ui/dist
```

默认地址 `http://127.0.0.1:8787`。`daemon` 同时运行调度器和 Web；`web` 只运行控制台，可编辑计划但不会执行计划。两者都要求至少 32 字节的环境凭据，不提供匿名生产模式。凭据不会写入配置、数据库或前端存储。浏览器使用 12 小时 HttpOnly / SameSite=Strict 会话；退出、服务重启使会话失效。登录失败限制为每分钟 10 次。插件使用相同凭据的 Bearer 认证，拥有管理员权限。

可在首次启动前配置：

```toml
[daemon]
publish_at = "09:00"
backup_at = "23:00"
utc_offset_minutes = 480
token_env = "MINERAL_WEB_TOKEN"
secure_cookie = false
```

`backup_at` 要求已有有效且启用的 `[backup]` 配置。没有备份目标时省略该行。固定 UTC 偏移 +480 为中国标准时间；不支持夏令时自动切换。对外提供服务时，通过 HTTPS 反向代理访问，并设置 `secure_cookie = true`。服务本身不终止 TLS。

时间配置仅初始化 `state/service.sqlite3` 中尚不存在的计划。后续在 Web「调度计划」编辑时间、偏移和开关，保存后下一次检查生效，不必重启。TOML 变更不会覆盖已经在 Web 保存的计划。

调度器每 10 秒检查一次；以「任务种类 + 该任务时区中的日历日期」持久化去重。当天执行时间已过但尚未触发时会补执行，不补跑之前日期。发布和备份同时到期时串行执行；工作区忙则等待下一次检查。当天任务失败不会无限自动重试，检查详情后可手动执行。修改时间不会让同一天已触发的任务再次触发。

触发前先持久化 claim。若进程在记录触发与启动任务之间崩溃，该日期保留为不确定状态，不自动重试。这是“至多一次自动派发”，不承诺外部 Git、数据库和进程之间的分布式 exactly-once。中断后应先核对远端和已有不可变发布/备份 intent，再决定后续操作。

一个工作区只允许一个 Web/daemon 进程，使用独立 SQLite 排他事务作为进程锁；进程崩溃后锁自动释放。CLI 的发布、备份和人工决定也遵守此锁；daemon 运行时请通过认证 API/Web 提交这些操作。SIGINT/SIGTERM 停止调度、关闭监听并等待已经接收的任务完成。

## 控制台

- 总览：数据源、待审数量、发布和备份操作。
- 调度计划：每天发布时间和备份时间、启停、UTC 偏移、触发记录。
- 执行历史：最近 200 次执行，按种类筛选；查看完整阶段、逐文件审核、目标文件及内容身份、Git base/tree/commit、备份 LFS 汇总、结果及错误。步骤可搜索、折叠；每条记录带时间。历史保存在数据库中，浏览列表限制不会删除旧记录。
- 人工审核：复用不可变审核 attempt 的批准/拒绝流程。导航待审通知和浏览器标题每 4 秒更新；等待人工审核的发布可在执行结果中识别。人工决定不会自动绕过审核或直接提交，可在决定完成后重新执行发布。
- 备份与验证：手动备份、初始化备份分支、验证远端备份，并查看完整执行记录。
- 插件与提交：展示认证触发协议。插件触发既有数据源的发布/备份流程；此版本不提供文件上传或任意插件代码执行。

执行记录中的 `succeeded` 表示应用调用正常返回。具体业务结果以 `outcome.result` 为准，例如 `waiting_for_human_review`、远端冲突、Noop，不能仅凭调用成功推断发布成功。进程中断时未完成的执行会标记为 `interrupted`，不会伪装为成功。

通知目前为站内通知与页面标题计数，不发送邮件、Webhook 或系统推送。

## API

除 `/api/v1/auth/login` 和 `/api/v1/auth/session` 外，API 均要求认证。Cookie 认证的非只读请求另需 `X-Mineral-Request: 1`；Bearer 请求可直接由插件发起。未认证返回 401，并发任务冲突返回 409。

```http
POST /api/v1/operations/publish
Authorization: Bearer <token>
```

响应包含 `operation_id`；可查询 `/api/v1/operations/<id>` 或订阅 `/events`。operation ID 用于实时进程，执行步骤中的 `Execution history: <UUID>` 指向跨重启的历史。

```text
GET  /api/v1/history
GET  /api/v1/history/<UUID>/steps
GET  /api/v1/schedules
POST /api/v1/schedules
POST /api/v1/operations/backup
POST /api/v1/operations/backup/init
POST /api/v1/operations/backup/verify
```

保存计划的 JSON：

```json
{"kind":"publish","at":"09:00","enabled":true,"utc_offset_minutes":480}
```

## systemd

提供 [mineral.service](../deploy/mineral.service)。部署时创建 `mineral` 用户，将二进制、构建好的前端、配置和凭据文件放入 unit 指定位置，或调整 unit 路径。配置中的相对路径相对于配置文件目录，建议生产配置使用绝对路径。确保状态目录、CAS、Git 仓库和资产目标目录对服务用户可写。

`/etc/mineral/environment` 包含 `MINERAL_WEB_TOKEN=...` 及已有 AI/R2/LFS 环境凭据，权限设为 0600。将 unit 安装到 `/etc/systemd/system/mineral.service` 后执行：

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now mineral
sudo journalctl -u mineral -f
```

服务默认自动重启失败的进程，正常停止会等候任务结束。执行日志与调度记录保存在工作区数据库；systemd 日志只记录服务级事件。此实现不自动清理历史数据库，部署者应将状态目录纳入容量规划和备份。
