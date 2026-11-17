"""清理线上验收留下的测试数据。"""
import http.cookiejar
import json
import urllib.parse
import urllib.request

BASE = "http://<SERVER_HOST>:18090"
ADMIN = "<ADMIN_PASSWORD>"

jar = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
data = urllib.parse.urlencode({"password": ADMIN}).encode()
opener.open(urllib.request.Request(BASE + "/login", data=data,
              headers={"Content-Type": "application/x-www-form-urlencoded"}), timeout=20)

r = opener.open(BASE + "/api/admin/billing/users?page_size=200", timeout=20)
d = json.loads(r.read())
items = (d.get("data") or {}).get("items") or []
print("现有用户:", [(u["id"], u["username"]) for u in items])

for u in items:
    name = u.get("username", "")
    if name.startswith("upgradetest"):
        print("  待清理:", u["id"], name)
        # 删 api key
        r = opener.open(BASE + "/api/admin/billing/api-keys?page_size=200", timeout=20)
        kd = json.loads(r.read())
        for k in (kd.get("data") or {}).get("items") or []:
            if k.get("user_id") == u["id"]:
                req = urllib.request.Request(
                    BASE + f"/api/admin/billing/api-keys/{k['id']}", method="DELETE")
                try:
                    opener.open(req, timeout=20)
                    print("    删 key", k["id"])
                except Exception as e:
                    print("    删 key 失败", e)
