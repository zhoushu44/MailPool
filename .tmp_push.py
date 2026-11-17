"""测试 git smart-http 端点是否可达（用 urllib 走 requests 同样的栈）。"""
import urllib.request

for url in [
    "https://github.com/zhoushu44/MailPool.git/info/refs?service=git-upload-pack",
    "https://github.com/",
]:
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "git/2.45.0"})
        r = urllib.request.urlopen(req, timeout=20)
        print("OK  ", url, r.status, len(r.read()))
    except Exception as e:
        print("FAIL", url, type(e).__name__, str(e)[:100])
