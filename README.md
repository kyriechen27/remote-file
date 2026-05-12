# Remote File

一个受 `dufs` 启发的轻量文件系统原型，重点放在更友好的界面、用户权限管理、管理员后台，以及公开文件直链访问。

## 功能

- 文件浏览、下载、上传、删除
- 登录会话和管理员后台
- 管理员后台可查看最近活动记录，包括登录、文件请求和管理操作
- 用户目录授权，可限制用户只能访问指定目录
- 新建普通用户默认目录为 `/user/用户名`，只有管理员默认拥有根目录 `/`
- 普通用户登录后默认进入自己的 `/user/用户名` 目录
- 系统启动时自动创建 `/public`，公开文件会复制到 `/public/用户名` 后再生成免登录直链
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

## 公开直链

用户可以把自己有权限访问的文件设为公开。系统会先把文件复制到 `/public/用户名`，再为这个公开副本生成链接，形如：

```text
http://127.0.0.1:8080/public/用户名/file.ext
```

该链接不需要登录即可访问。取消公开后，公开副本会被移除，原文件仍保留在原目录。

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
