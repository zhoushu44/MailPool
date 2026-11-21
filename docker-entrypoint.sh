#!/bin/sh
# ---------------------------------------------------------------------------
# 入口脚本：修正数据目录权限后降权运行
#
# 为什么需要它：
#   镜像以非 root（UID 1000）运行，但用户挂载的宿主目录往往属于 root。
#   直接启动会因为写不了 SQLite 而 ExitCode=3 崩溃：
#     sqlite3.OperationalError: unable to open database file
#   让用户先手动 chown 是反人性的，所以这里以 root 启动、修正属主、
#   再用 gosu 降权到 mailpool 用户运行应用。
#
# 只 chown 数据目录，不递归碰其它路径。
# ---------------------------------------------------------------------------
set -e

DATA_DIR="${MAILPOOL_DATA:-/app/data}"

if [ "$(id -u)" = "0" ]; then
    # 以 root 启动：确保数据目录存在且属主正确，然后降权
    mkdir -p "$DATA_DIR"
    chown -R mailpool:mailpool "$DATA_DIR" 2>/dev/null || true
    exec gosu mailpool "$@"
fi

# 已经是非 root（比如用户指定了 --user），直接运行
exec "$@"
