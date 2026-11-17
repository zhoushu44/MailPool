"""查 GitHub Actions 运行状态（走服务器 SOCKS 代理）。"""
import json
import subprocess
import urllib.request

PROXY = "socks5h://<PROXY_USER>:<PROXY_PASSWORD>@<SERVER_HOST>:7890"

cred = "protocol=https\nhost=github.com\n\n"
p = subprocess.run(["git", "credential", "fill"], input=cred, capture_output=True, text=True)
token = ""
for line in p.stdout.splitlines():
    if line.startswith("password="):
        token = line.split("=", 1)[1]

import requests

s = requests.Session()
s.trust_env = False
H = {
    "Authorization": f"Bearer {token}",
    "Accept": "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
}
r = s.get("https://api.github.com/repos/zhoushu44/MailPool/actions/runs?per_page=5",
          headers=H, proxies={"http": PROXY, "https": PROXY}, timeout=30)
d = r.json()
print("HTTP", r.status_code)
for run in d.get("workflow_runs", []):
    print(f"  {run['name']} | {run['status']} | {run['conclusion']} | "
          f"{run['head_sha'][:8]} | {run['created_at']} | {run['html_url']}")
