# Remote File

一个受 `dufs` 启发的轻量文件系统原型，重点放在更友好的界面、用户权限管理、管理员后台，以及公开文件直链访问。

## 功能

- 文件浏览、下载、上传、删除
- 登录会话和管理员后台
- 管理员后台可查看最近活动记录，包括登录、文件请求和管理操作
- 用户目录授权，可限制用户只能访问指定目录
- 新建普通用户默认目录为 `/user/用户名`，只有管理员默认拥有根目录 `/`
- 普通用户登录后默认进入自己的 `/user/用户名` 目录
- 文件公开后会生成免登录直链，不会复制原文件占用额外存储
- 用户能力开关：上传、删除、公开文件
- 文件可复制登录后下载链接，登录且有权限的用户可访问
- 文件可授权给指定用户，被授权用户登录后可下载该文件
- 文件公开后可通过 `/public/...` 直链免登录访问
- 文件列表显示下载次数，登录下载和公开直链下载都会计数
- 状态持久化到本地 JSON，不需要额外数据库

## 运行

```bash
cargo run
```

默认访问地址：

```text
http://127.0.0.1:8080
```

默认管理员：

```text
admin / admin
```

首次启动后请在后台修改管理员密码。

## 环境变量

```text
REMOTE_FILE_BIND=127.0.0.1:8080
REMOTE_FILE_ROOT=storage/files
REMOTE_FILE_DATA=storage/meta
REMOTE_FILE_UPLOAD_LIMIT_BYTES=10737418240
```

`REMOTE_FILE_ROOT` 是实际文件根目录，`REMOTE_FILE_DATA` 保存用户、会话和公开文件记录。`REMOTE_FILE_UPLOAD_LIMIT_BYTES` 控制单次请求上传大小上限，默认 10GB。

## Docker Compose

本地源码构建运行：

客户部署，直接拉取已发布镜像：

```bash
docker compose up -d
```

本地源码编译运行：

```bash
docker compose -f compose.local.yaml up -d --build
```

Compose 默认暴露：

```text
http://127.0.0.1:8080
```

默认挂载目录：

```text
./storage/files -> /data/files
./storage/meta  -> /data/meta
```

停止服务：

```bash
docker compose down
```

## 发布 Docker 镜像

先登录 Docker Hub：

```bash
docker login
```

构建并推送镜像：

```bash
docker build -t coketeatt/remote-file:latest .
docker push coketeatt/remote-file:latest
```

如果你要同时支持 Apple Silicon 和普通 Linux 服务器，可以用 buildx 发布多架构镜像：

```bash
docker buildx create --use
docker buildx build --platform linux/amd64,linux/arm64 \
  -t coketeatt/remote-file:latest \
  --push .
```

发布后，用户只需要下载 [docker-compose.yaml](./docker-compose.yaml) 并运行：

```bash
docker compose up -d
```

更新到最新版：

```bash
docker compose pull
docker compose up -d
```

## Cloudflare Pages + R2 部署

本项目现在也支持像 `ljxi/Cloudflare-R2-oss` 一样直接部署到 Cloudflare Pages。Cloudflare 版本使用：

- `public/`：静态前端页面
- `functions/[[path]].js`：Pages Functions API
- R2 绑定 `BUCKET`：保存上传文件、用户状态、会话、公开链接和审计日志

Cloudflare 版本和 Rust/Docker 版本共用前端交互和 API 路径，但存储后端不同：Cloudflare 版本没有本地文件系统，全部数据都会写入 R2。

Cloudflare Pages Functions 单次请求体有平台上限。Cloudflare 版本会对超过 64MB 的文件自动使用 R2 multipart upload 分片上传，避免大文件直接上传时出现 `413 Payload Too Large`。

### 通过 Cloudflare 控制台部署

1. Fork 或推送本仓库到 GitHub。
2. 在 Cloudflare R2 新建存储桶，例如 `remote-file`。
3. 在 Cloudflare Pages 新建项目，连接该 Git 仓库。
4. 构建设置保持简单：

```text
Build command: 留空
Build output directory: public
```

5. 部署后进入 Pages 项目设置，打开 `Functions` -> `R2 bucket bindings`，添加绑定：

```text
Variable name: BUCKET
R2 bucket: remote-file
```

6. 重新部署 Pages 项目。

首次访问后会自动初始化默认管理员：

```text
admin / admin
```

首次登录后请立即在后台修改管理员密码。

### 本地调试 Cloudflare 版本

启动本地 R2 预览环境：

```bash
npm run dev:cloudflare
```

默认会通过 Wrangler Pages Dev 启动本地 Cloudflare Pages Functions 环境，并把 R2 绑定命名为 `BUCKET`。

### 命令行部署

确认已经登录 Wrangler：

```bash
npx wrangler login
```

创建 R2 存储桶：

```bash
npx wrangler r2 bucket create remote-file
```

部署 Pages：

```bash
npm run deploy:cloudflare
```

如果你的 R2 存储桶不是 `remote-file`，请同步修改 [wrangler.toml](./wrangler.toml) 里的 `bucket_name`。

## 公开直链

用户可以把自己有权限访问的文件设为公开。系统会为原文件生成免登录直链，形如：

```text
http://127.0.0.1:8080/public/path/to/file.ext
```

如果源文件位于普通用户目录，例如 `/user/kyriechen/tools/file.ext`，公开直链会隐藏用户目录前缀，生成为 `/public/tools/file.ext`。

该链接不需要登录即可访问。取消公开后，直链失效，原文件仍保留在原目录。

## 登录链接和文件授权

每个文件都可以复制普通下载链接：

```text
http://127.0.0.1:8080/api/files/path/to/file.ext
```

这个链接需要先登录。用户能下载的前提是满足任意一种条件：

- 管理员
- 用户目录权限覆盖该文件
- 文件被单独授权给该用户

如果用户直接在浏览器打开该链接且当前未登录，浏览器会弹出用户名和密码输入框。

文件行里的“授权”按钮可以输入用户名，多个用户用逗号分隔。留空保存会清空该文件的单独授权。

## 后续建议

- 用 Argon2 替换当前的盐化 SHA-256 密码哈希
- 增加 HTTPS 反代部署文档
- 将 JSON 存储替换为 SQLite，支持审计日志和更强并发
- 增加分享链接过期时间、访问次数限制和下载统计
