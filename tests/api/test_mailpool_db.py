# -*- coding: utf-8 -*-
"""MailPool 数据库验证：迁移、刷新历史、标签/代理字段落地。"""
import sqlite3

DB = r"C:\Users\zs\Desktop\MailPool\data\mailpool.db"
conn = sqlite3.connect(DB)
PASS, FAIL = 0, 0

def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        print(f"  FAIL  {name}  -> {detail}")

# 1. email_accounts 表结构
cols = [r[1] for r in conn.execute("PRAGMA table_info(email_accounts)")]
check("email_accounts 含 tags", "tags" in cols, cols)
check("email_accounts 含 proxy", "proxy" in cols, cols)
check("email_accounts 含 remark", "remark" in cols, cols)

# 2. token_refresh_logs 表
tabs = [r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")]
check("token_refresh_logs 表存在", "token_refresh_logs" in tabs, tabs)

# 3. token_refresh_logs 结构
if "token_refresh_logs" in tabs:
    lcols = [r[1] for r in conn.execute("PRAGMA table_info(token_refresh_logs)")]
    for f in ("id", "trigger", "provider", "status", "total", "success", "failed", "skipped", "duration", "detail", "created_at"):
        check(f"logs 含字段 {f}", f in lcols, lcols)

# 4. 刷新历史记录内容
logs = list(conn.execute("SELECT id, trigger, status, total, success, failed, skipped, duration FROM token_refresh_logs ORDER BY id DESC LIMIT 5"))
check("有刷新历史记录", len(logs) > 0, logs)
if logs:
    recent = logs[0]
    check("最新记录 status=finished", recent[2] == "finished", recent)
    check("最新记录 numbers >= 0", all(isinstance(x, int) and x >= 0 for x in recent[3:7]), recent)

# 5. 账号标签/代理数据（测试残留清理状态）
rows = list(conn.execute("SELECT email, tags, proxy, remark FROM email_accounts WHERE tags != '' OR proxy != '' OR remark NOT IN ('', 'autotest') LIMIT 5"))
for r in rows:
    check(f"账号 {r[0][:20]} tags/proxy 可读写", isinstance(r[1], str) and isinstance(r[2], str), r)

# 6. 账号数量一致性
n = conn.execute("SELECT COUNT(*) FROM email_accounts").fetchone()[0]
check("账号总数 > 0", n > 0, n)
n_h = conn.execute("SELECT COUNT(*) FROM email_accounts WHERE health_status='healthy'").fetchone()[0]
print(f"  INFO  总账号 {n} / 健康 {n_h}")

conn.close()
print(f"\n结果: PASS {PASS} / FAIL {FAIL}")
exit(1 if FAIL else 0)