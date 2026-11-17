"""线上验收（修正路径）：全部页面 + 真实计费流程。"""
import http.cookiejar
import json
import urllib.parse
import urllib.request

BASE = "http://<SERVER_HOST>:18090"
ADMIN = "<ADMIN_PASSWORD>"

jar = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))

# 登录
data = urllib.parse.urlencode({"password": ADMIN}).encode()
opener.open(urllib.request.Request(BASE + "/login", data=data,
              headers={"Content-Type": "application/x-www-form-urlencoded"}), timeout=20)

print("=== 全部页面（修正路径） ===")
PAGES = ["/", "/overview", "/accounts", "/projects", "/pool", "/buy-email",
         "/orders", "/balances", "/recharges", "/billing/users", "/billing/cards",
         "/account", "/settings", "/docs", "/docs/quick-start",
         "/docs/temporary-email", "/docs/api-overview", "/docs/errors"]
bad = []
for p in PAGES:
    try:
        r = opener.open(BASE + p, timeout=25)
        n = len(r.read())
        print(f"  OK  {p} → {r.status} ({n} bytes)")
    except Exception as e:
        bad.append(p)
        print(f"  FAIL {p} → {type(e).__name__} {str(e)[:60]}")

print("\n=== 计费管理 API ===")
for p in ["/api/admin/billing/users", "/api/admin/billing/orders",
          "/api/admin/billing/config", "/api/admin/billing/api-keys",
          "/api/admin/billing/cards/query?card_key=NOPE"]:
    try:
        r = opener.open(BASE + p, timeout=20)
        print(f"  OK  {p} → {r.status}")
    except Exception as e:
        print(f"  FAIL {p} → {type(e).__name__} {str(e)[:70]}")

print("\n=== 真流程：建用户 → 签 Key → 调余额 → 生成充值卡 ===")
req = urllib.request.Request(
    BASE + "/api/admin/billing/users",
    data=json.dumps({"username": "upgradetest", "password": "upgradepass"}).encode(),
    headers={"Content-Type": "application/json"}, method="POST")
r = opener.open(req, timeout=20)
d = json.loads(r.read())
uid = (d.get("data") or {}).get("id") or (d.get("data") or {}).get("user", {}).get("id")
print(f"  建用户 → {r.status}  id={uid}")

req = urllib.request.Request(
    BASE + f"/api/admin/billing/users/{uid}/balance",
    data=json.dumps({"amount": "12.34", "remark": "升级验收"}).encode(),
    headers={"Content-Type": "application/json"}, method="POST")
r = opener.open(req, timeout=20)
print(f"  调余额 → {r.status} {r.read().decode()[:120]}")

req = urllib.request.Request(
    BASE + "/api/admin/billing/cards/generate",
    data=json.dumps({"amount": "5.00", "count": 2}).encode(),
    headers={"Content-Type": "application/json"}, method="POST")
r = opener.open(req, timeout=20)
cd = json.loads(r.read())
print(f"  生成卡 → {r.status} {json.dumps(cd, ensure_ascii=False)[:220]}")

req = urllib.request.Request(BASE + f"/api/admin/billing/users/{uid}/logs")
r = opener.open(req, timeout=20)
ld = json.loads(r.read())
print(f"  查流水 → {r.status} {json.dumps(ld, ensure_ascii=False)[:250]}")

print("\n=== 客户侧：登录 → 看余额 → 兑换充值卡 ===")
jar2 = http.cookiejar.CookieJar()
op2 = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar2))
data = urllib.parse.urlencode({"username": "upgradetest", "password": "upgradepass"}).encode()
r = op2.open(urllib.request.Request(BASE + "/user/login", data=data,
              headers={"Content-Type": "application/x-www-form-urlencoded"}), timeout=20)
print(f"  客户登录 → {r.status} {r.url}")

r = op2.open(BASE + "/recharges", timeout=20)
body = r.read().decode("utf-8", "replace")
import re
flat = re.sub(r"\s+", "", body)
m = re.search(r"总余额：([\d.]+)元", flat)
print(f"  /recharges → {r.status}  总余额={m.group(1) if m else '?'}")

print("\n结论:", "❌ 异常页面: " + str(bad) if bad else "✅ 全部页面正常")
