"""看 CI 那次 run 推了哪些镜像标签（只看 push 步骤的 tag 行）。"""
import subprocess

import requests

PROXY = "socks5h://<PROXY_USER>:<PROXY_PASSWORD>@<SERVER_HOST>:7890"
RUN_ID = "44149022655"

cred = "protocol=https\nhost=github.com\n\n"
p = subprocess.run(["git", "credential", "fill"], input=cred, capture_output=True, text=True)
token = ""
for line in p.stdout.splitlines():
    if line.startswith("password="):
        token = line.split("=", 1)[1]

s = requests.Session()
s.trust_env = False
H = {"Authorization": f"Bearer {token}",
     "Accept": "application/vnd.github+json",
     "X-GitHub-Api-Version": "2022-11-28"}
PR = {"http": PROXY, "https": PROXY}

jobs = s.get(f"https://api.github.com/repos/zhoushu44/MailPool/actions/runs/{RUN_ID}/jobs",
             headers=H, proxies=PR, timeout=30).json().get("jobs", [])
for j in jobs:
    if j["name"] != "Build & Push":
        continue
    r = s.get(f"https://api.github.com/repos/zhoushu44/MailPool/actions/jobs/{j['id']}/logs",
              headers=H, proxies=PR, timeout=60)
    for line in r.text.splitlines():
        if "/mailpool:" in line and "tonistiigi" not in line:
            print(line[:250])
