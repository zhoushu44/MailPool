# MailPool 部署记录

> 本文是部署流程记录，**不含任何真实服务器地址与口令**。
> 自建部署时把下文 `<SERVER_HOST>` / `<ADMIN_PASSWORD>` 换成自己的值。

## 访问信息

| 项 | 值 |
|---|---|
| **访问地址** | `http://<SERVER_HOST>:18090/` |
| 管理口令 | 由环境变量 `MAILPOOL_ADMIN_PASSWORD` 提供（不要写进仓库） |
| 服务器 | 建议 4 核 / 8G 内存以上，Linux + Docker |
| 端口 | 18090 → 容器 18080（需在防火墙/安全组放行） |
| 容器 | `mailpool`（`--restart unless-stopped`，开机自启） |
| 项目目录 | `/opt/mailpool` |

## 首次部署

```bash
# 1. 准备数据目录
mkdir -p /opt/mailpool/data

# 2. 启动容器（口令通过环境变量传入）
docker run -d --name mailpool --restart unless-stopped \
  -p 18090:18080 \
  -v /opt/mailpool/data:/app/data \
  -e MAILPOOL_ADMIN_PASSWORD='<ADMIN_PASSWORD>' \
  -e MAILPOOL_DATA=/app/data \
  zhoushu44/mailpool:1.0
```

## 升级

```bash
export MAILPOOL_ADMIN_PASSWORD='<ADMIN_PASSWORD>'
bash /opt/mailpool/upgrade.sh
```

`upgrade.sh` 会：备份数据库 → 拉取新镜像 → 重建容器 → 探活 → 失败自动回滚。

## 环境变量

| 变量 | 说明 | 必填 |
|---|---|---|
| `MAILPOOL_ADMIN_PASSWORD` | 管理后台口令。不设时仅允许本机回环访问 | 生产必填 |
| `MAILPOOL_DATA` | 数据目录（SQLite 数据库、日志） | 否，默认 `./data` |
| `MAILPOOL_PORT` | 容器内监听端口 | 否，默认 18080 |
| `MAILPOOL_SESSION_SECRET` | 会话签名密钥 | 建议设置 |

## 数据备份

数据库是单文件 SQLite（WAL 模式），备份时注意连 `-wal` / `-shm` 一起：

```bash
sqlite3 /opt/mailpool/data/mailpool.db ".backup '/opt/mailpool/data/backup.db'"
```

## 健康检查

```bash
curl -s http://127.0.0.1:18090/api/v1/health
```

返回 `{"code":"00000", ...}` 即正常。

## 计费版升级记录

计费体系包含：充值（扫码 + 充值卡）、按项目单价冻结、收码实扣、释放解冻。相关配置在「设置 → 计费」页面维护，开关为 `billing.enabled`。
