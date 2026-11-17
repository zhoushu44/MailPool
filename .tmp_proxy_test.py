"""测两条路：本地走服务器 SOCKS 代理能不能通 GitHub。"""
import socket
import urllib.request

PROXY_HOST, PROXY_PORT = "<SERVER_HOST>", 7890

# 1. 代理端口本机能不能连
try:
    socket.create_connection((PROXY_HOST, PROXY_PORT), timeout=8)
    print(f"OK    代理端口 {PROXY_HOST}:{PROXY_PORT} 可连")
except Exception as e:
    print(f"FAIL  代理端口 {PROXY_HOST}:{PROXY_PORT} → {str(e)[:60]}")
    raise SystemExit(0)

# 2. 用 requests 走 socks5 代理
try:
    import requests
    proxies = {
        "http": "socks5h://<PROXY_USER>:<PROXY_PASSWORD>@<SERVER_HOST>:7890",
        "https": "socks5h://<PROXY_USER>:<PROXY_PASSWORD>@<SERVER_HOST>:7890",
    }
    s = requests.Session()
    s.trust_env = False
    r = s.get("https://api.github.com", proxies=proxies, timeout=25)
    print(f"OK    经代理访问 api.github.com → {r.status_code}")
    r2 = s.get("https://github.com/zhoushu44/MailPool.git/info/refs?service=git-upload-pack",
               proxies=proxies, timeout=25,
               headers={"User-Agent": "git/2.45.0"})
    print(f"OK    经代理访问 git smart-http → {r2.status_code} ({len(r2.content)} bytes)")
except Exception as e:
    print(f"FAIL  经代理访问 → {type(e).__name__}: {str(e)[:120]}")
