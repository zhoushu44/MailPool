import sqlite3

DB = "/app/data/mailpool.db"
c = sqlite3.connect(DB)

print("=== 迁移前表清单 ===")
tabs = [r[0] for r in c.execute(
    "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
print(" ", tabs)

BILLING = ["users", "api_keys", "recharge_orders", "redeem_cards",
           "email_orders", "balance_logs"]
missing = [t for t in BILLING if t not in tabs]
print("缺失的计费表:", missing if missing else "无（迁移会自动建）")

print("\n=== 关键：确认老表结构没被新代码改动 ===")
for t in ("email_accounts", "email_usages", "mail_records", "projects", "settings"):
    if t in tabs:
        cols = [r[1] for r in c.execute(f"PRAGMA table_info({t})")]
        print(f"  {t}: {len(cols)} 列")

print("\n=== 现有数据量（迁移不能动） ===")
for t in ("email_accounts", "projects", "mail_records", "email_usages", "settings"):
    if t in tabs:
        print(f"  {t}: {c.execute('select count(*) from ' + t).fetchone()[0]}")

print("\n=== settings 现有 key ===")
for row in c.execute("select key from settings order by key"):
    print("  ", row[0])
c.close()
