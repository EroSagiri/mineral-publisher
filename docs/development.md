# 开发环境

## 前端（`web-ui/`）

Web UI 是一个独立的 browser application，放在仓库根的 `web-ui/`，不是 `mineral-host`
的 Rust 源码。以后即使拆出单独 server、桌面壳或远程部署，也不用把它从 crate 目录里搬出来。

```bash
# 生产：构建一次，由 mineral web 同源提供
npm --prefix web-ui install
npm --prefix web-ui run build      # -> web-ui/dist

# 开发：Vite dev server 把 /api 代理到 mineral web
mineral web                        # 127.0.0.1:8787
npm --prefix web-ui run dev        # 127.0.0.1:5173
```

开发与生产都是**同源**，所以没有 CORS，也没有需要配置的 API base URL：

```text
生产  http://127.0.0.1:8787/          + /api/v1/...
开发  http://127.0.0.1:5173/          + /api/... → 代理到 8787
```

### `cargo` 不会调用 npm

`cargo build` 不依赖 Node，也不跑 `build.rs` 去调 npm。那会让 Rust CI 和前端构建绑死，
收益不大。`web-ui/dist` 是构建产物（gitignored），服务器找不到它时仍然提供完整 API，
并对页面返回一段说明而不是一堆 404。

`npm --prefix web-ui run typecheck` 只做类型检查；`electron`/打包之类的留到以后的
packaging 阶段再决定是外置 `dist` 还是编进 binary。

## dev profile 为什么关掉 debug info

```toml
[profile.dev]
debug = "line-tables-only"

[profile.dev.package."*"]
debug = false
```

**Full dependency debuginfo exhausted the development environment disk after adding the
HTTP stack.** 加入 axum + tokio 之后，`target/debug` 的 debug info 达到 **10.5G**，把这台
50G 的机器打满两次，编译直接以 `ENOSPC` 中断（不是"慢"，是做不下去）。

取舍：

- workspace 自己的 crate 保留 `file:line`，**panic 与 backtrace 仍然指向准确的文件和行号**；
  日常开发、测试失败、CI 日志需要的就是这个。
- 依赖不带 debug info —— 实际上几乎不会有人在调试器里单步进 `tokio` 或 `image`。

如果哪天需要在调试器里看依赖的变量，临时用环境变量覆盖即可，不需要改文件：

```bash
CARGO_PROFILE_DEV_DEBUG=2 CARGO_PROFILE_DEV_PACKAGE_DEBUG=2 cargo build
```

## 门禁

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo check -p mineral-core --target wasm32-unknown-unknown
npm --prefix web-ui run typecheck
npm --prefix web-ui run build
./scripts/verify-cli-independence.sh
```

前四条与最后一条是 Rust 侧的（最后一条证明删掉 `src/cli/` 之后 application / operations /
web 三层仍然完整可用）。前端两条需要 Node，刻意与 Rust 门禁分开。

## 已知的环境注意

- `git-lfs` 装在 `~/.local/bin`，非交互 shell 的 PATH 里没有它。
- 到 GitHub 的 HTTPS 推送不稳定，推送用 SSH 显式 URL：
  `git push git@github.com:EroSagiri/mineral-publisher.git HEAD:main`。
- 到 GitHub 的 LFS *传输*端点在当前网络下吞吐为 0（见 `core-split-notes.md` §2.4），
  这不影响 Git 协议本身，也不影响本项目的开发与测试。
