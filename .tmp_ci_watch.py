"""轮询 GitHub Actions 运行状态，直到完成。"""
import subprocess
import sys
import time

import requests

PROXY = "socks5h://<PROXY_USER>:<PROXY_PASSWORD>@<SERVER_HOST>:7890"
RUN_ID = sys.argv[1] if len(sys.argv) > 1 else "44149022655"

cred = "protocol=https\nhost=github.com\n\n"
p = subprocess.run(["git", "credential", "fill"], input=cred, capture_output=True, text=True)
token = ""
for line in p.stdout.splitlines():
    if line.startswith("password="):
        token = line.split("=", 1)[1]

s = requests.Session()
s.trust_env = False
H = {
    "Authorization": f"Bearer {token}",
    "Accept": "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
}
PR = {"http": PROXY, "https": PROXY}

last = None
for _ in range(120):
    r = s.get(f"https://api.github.com/repos/zhoushu44/MailPool/actions/runs/{RUN_ID}",
              headers=H, proxies=PR, timeout=30)
    run = r.json()
    status, conclusion = run.get("status"), run.get("conclusion")
    jobs = s.get(f"https://api.github.com/repos/zhoushu44/MailPool/actions/runs/{RUN_ID}/jobs",
                 headers=H, proxies=PR, timeout=30).json().get("jobs", [])
    cur = (status, conclusion, tuple((j["name"], j["status"], j.get("conclusion")) for j in jobs))
    if cur != last:
        print(f"--- {status} / {conclusion}")
        for j in jobs:
            print(f"    {j['name']}: {j['status']} / {j.get('conclusion')}")
        last = cur
    if status == "completed":
        print(f"\n=== 最终结果: {conclusion} ===")
        sys.exit(0 if conclusion == "success" else 1)
    time.sleep(15)
print("超时")
sys.exit(2)
