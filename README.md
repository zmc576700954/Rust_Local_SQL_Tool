# Rust Local SQL Tool

以 Rust 为核心的本地 SQL 工具工程，包含后端服务、前端 Web UI 以及端到端测试 runner。

## 目录结构

- core_lib：核心库（数据库能力、同步/传输、AI 规划等）
- web-server：后端服务（Axum），可同时提供 API 与静态 Web UI
- web-ui：前端 Web UI（React + TypeScript + Vite）
- e2e-runner：真实环境 E2E runner
- docs：设计与联调文档

## 快速开始

前置依赖：

- Rust toolchain（stable）
- Node.js（建议 20+）与 npm

开发模式（分别启动）：

```bash
cargo run -p web-server
```

```bash
cd web-ui
npm ci
npm run dev
```

生产构建（生成 web-ui/dist 后由后端托管）：

```bash
cd web-ui
npm ci
npm run build
```

```bash
cargo run -p web-server
```

默认静态目录为 `web-ui/dist`，可通过环境变量覆盖：

```bash
WEB_UI_DIST_DIR=web-ui/dist
```

## E2E

参考 [docs/e2e-real-env.md](docs/e2e-real-env.md)。

## 仓库策略（GitHub 主仓库，Gitee 镜像）

主仓库（GitHub）：

- https://github.com/zmc576700954/Rust_Local_SQL_Tool.git

镜像仓库（Gitee）：

- https://gitee.com/zhu_ming_chen/rust-local-sql-tool.git

建议镜像方式：

- 以 GitHub 为唯一写入口（合并 PR、打 Tag、发 Release）
- 由 GitHub Actions 自动把变更镜像推送到 Gitee

启用镜像需要在 GitHub 仓库配置 Secrets：

- `GITEE_SSH_PRIVATE_KEY`：有 Gitee 推送权限的 SSH 私钥（建议专用 deploy key）

Gitee 侧建议配置方式：

- 在 Gitee 仓库添加 Deploy Key（勾选允许写入）
- 将对应私钥内容保存到 GitHub Actions Secret：`GITEE_SSH_PRIVATE_KEY`

## 仓库整洁

根目录 `.gitignore` 已排除 Rust 编译产物与前端缓存/构建产物（例如 `target/`、`web-ui/node_modules/`、`web-ui/dist/`）。

如果这些目录曾经被提交到仓库历史中，需要在本地执行一次取消跟踪后再提交：

```bash
git rm -r --cached target web-ui/dist web-ui/node_modules
```

本地 remote 建议（仅示例）：

```bash
git remote add origin https://github.com/zmc576700954/Rust_Local_SQL_Tool.git
git remote add gitee https://gitee.com/zhu_ming_chen/rust-local-sql-tool.git
```
