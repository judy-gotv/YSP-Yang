# YSP-Yang

央视频直播代理（Rust 版，`iptv-rust`），多阶段构建打出极小 `scratch` 运行时镜像。

## 接口

| 路由 | 说明 |
|---|---|
| `/list.m3u` | 订阅清单 |
| `/live/<ch>.m3u8` | 频道播放列表 |
| `/segment/<ch>/<id>.ts` | 切片 |
| `/channels` | 频道表（JSON） |
| `/health` | 健康检查 |

## Docker

镜像由 GitHub Actions 自动构建并发布到 GHCR（`linux/amd64` + `linux/arm64`），main 分支每次推送自动重建：

```bash
docker pull ghcr.io/judy-gotv/ysp-yang:latest

注意  注意 一点要先拉取 
curl -o /opt/channels.yaml https://raw.githubusercontent.com/judy-gotv/YSP-Yang/main/channels.yaml

docker run -d --name ysp-yang -p 8787:8787 ghcr.io/judy-gotv/ysp-yang:latest
```

跑起来后订阅地址：`http://<host>:8787/list.m3u`

自定义端口 / 频道表：

```bash
docker run -d --name ysp-yang -p 19980:8787 \
  -v /opt/channels.yaml:/app/channels.yaml \
  ghcr.io/judy-gotv/ysp-yang:latest

```

## 本地构建

```bash
docker build -f Dockerfile.rust -t ysp-yang .
```
