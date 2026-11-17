"""在生产库副本上彩排升级：跑 init_db() 迁移，然后核对数据没丢。"""
import os
import sqlite3
import sys

REPO = r"C:\Users\zs\Desktop\MailPool"
DATA = os.path.join(REPO, ".tmp_rehearsal", "data")
os.environ["MAILPOOL_DATA"] = DATA
os.environ["MAILPOOL_ADMIN_PASSWORD"] = "rehearsal-only"
sys.path.insert(0, REPO)

DB = os.path.join(DATA, "mailpool.db")


def snapshot():
    c = sqlite3.connect(DB)
    out = {}
    for t in ("email_accounts", "projects", "mail_records", "email_usages",
              "settings", "token_refresh_logs", "mail_baselines"):
        try:
            out[t] = c.execute("select count(*) from " + t).fetchone()[0]
        except Exception:
            out[t] = None
    c.close()
    return out


before = snapshot()
print("=== 迁移前 ===")
for k, v in before.items():
    print(f"  {k}: {v}")

from app.database import init_db  # noqa: E402

init_db()
print("\ninit_db() 执行完毕")

after = snapshot()
print("\n=== 迁移后 ===")
for k, v in after.items():
    mark = "✓" if before[k] == v else "✗ 变了！"
    print(f"  {k}: {v}  {mark}")

print("\n=== 新增的计费表 ===")
c = sqlite3.connect(DB)
tabs = [r[0] for r in c.execute(
    "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
for t in tabs:
    n = c.execute("select count(*) from " + t).fetchone()[0]
    print(f"  {t}: {n} 行")
c.close()

bad = [k for k in before if before[k] is not None and before[k] != after[k]]
print("\n结果:", "❌ 有数据被改动：" + str(bad) if bad else "✅ 老数据一行没动")
sys.exit(1 if bad else 0)
