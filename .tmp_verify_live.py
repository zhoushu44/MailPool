"""线上验收：登录 → 跑所有计费页面 → 检查计费表和接口。"""
import http.cookiejar
import json
import urllib.request

BASE = "http://<SERVER_HOST>:18090"
ADMIN = "<ADMIN_PASSWORD>"

jar = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))

PAGES = [
    "/login", "/dashboard", "/pool", "/projects", "/buy-email", "/orders",
    "/balances", "/recharges", "/billing/users", "/billing/cards",
    "/account", "/settings", "/docs/errors", "/user/login",
]

print("=== 1. 未登录访问（应该 303 → /login 或 /user/login） ===")
for p in ["/billing/users", "/recharges", "/balances"]:
    req = urllib.request.Request(BASE + p)
    try:
        r = opener.open(req, timeout=20)
        print(f"  {p} → {r.status} {r.url}")
    except urllib.error.HTTPError as e:
        print(f"  {p} → {e.code} {e.headers.get('Location','')}")

print("\n=== 2. 管理员登录 ===")
data = urllib.parse.urlencode({"password": ADMIN}).encode()
req = urllib.request.Request(BASE + "/login", data=data,
                             headers={"Content-Type": "application/x-www-form-urlencoded"})
r = opener.open(req, timeout=20)
print(f"  登录 → {r.status} {r.url}")
print("  cookie:", [c.name for c in jar])

print("\n=== 3. 登录后访问全部页面 ===")
bad = []
for p in PAGES:
    try:
        r = opener.open(BASE + p, timeout=25)
        ok = r.status == 200
        n = len(r.read())
        if not ok:
            bad.append(p)
        print(f"  {'OK ' if ok else 'FAIL'} {p} → {r.status} ({n} bytes)")
    except Exception as e:
        bad.append(p)
        print(f"  FAIL {p} → {type(e).__name__} {str(e)[:60]}")

print("\n=== 4. 计费 API ===")
for p in ["/api/admin/billing/users", "/api/admin/billing/cards",
          "/api/admin/billing/orders", "/api/admin/billing/config"]:
    try:
        r = opener.open(BASE + p, timeout=20)
        d = json.loads(r.read())
        print(f"  OK  {p} → {r.status}")
    except Exception as e:
        print(f"  FAIL {p} → {type(e).__name__} {str(e)[:80]}")

print("\n=== 5. 未授权的充值接口（应该 401） ===")
try:
    fresh = urllib.request.build_opener()
    r = fresh.open(BASE + "/api/v1/recharge", timeout=20)
    print(f"  FAIL /api/v1/recharge → {r.status}（应该 401）")
except urllib.error.HTTPError as e:
    print(f"  OK  /api/v1/recharge → {e.code}（未授权被拒）")

print("\n=== 6. 公开 API ===")
r = opener.open(BASE + "/api/product/info", timeout=30)
d = json.loads(r.read())
tmp = (d.get("data") or {}).get("temporary") or []
print(f"  /api/product/info → {r.status}，项目 {len(tmp)} 个")

print("\n结论:", "❌ 有页面异常: " + str(bad) if bad else "✅ 全部页面正常")
