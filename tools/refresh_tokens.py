"""一键补齐 Outlook 账号的 client_id 并刷新 refresh_token。

用法:
    python refresh_tokens.py --source <secured_tokens.txt 路径> [--interactive]

流程:
    1. 自动尝试用 Thunderbird 公开 client_id 刷新现有 refresh_token（无需交互）
    2. 刷新失败的账号: 走 Device Code Flow 交互授权（需浏览器登录）
    3. 输出 MailPool 可导入格式: email----password----client_id----refresh_token

输入文件格式(secured_tokens.txt):
    email---password---refresh_token---access_token---expire_at
输出文件: secured_tokens_mailpool.txt (与输入同目录)
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

# Thunderbird 桌面版公开 client_id（Mozilla OAuth2Providers 源码）
THUNDERBIRD_CLIENT_ID = "9e5f94bc-e8a4-4e73-b8be-63364c29d753"

# scope: IMAP + SMTP + offline_access（Thunderbird 客户端自带）
SCOPE = (
    "https://outlook.office.com/IMAP.AccessAsUser.All "
    "https://outlook.office.com/SMTP.Send "
    "offline_access"
)

AUTHORITY = "https://login.microsoftonline.com/common/oauth2/v2.0"
TOKEN_URL = f"{AUTHORITY}/token"
DEVICE_CODE_URL = f"{AUTHORITY}/devicecode"


class Account:
    __slots__ = ("email", "password", "refresh_token", "access_token", "expire_at")

    def __init__(self, email: str, password: str, refresh_token: str, access_token: str, expire_at: str):
        self.email = email
        self.password = password
        self.refresh_token = refresh_token
        self.access_token = access_token
        self.expire_at = expire_at

    def to_mailpool(self, client_id: str, new_refresh: str) -> str:
        return f"{self.email}----{self.password}----{client_id}----{new_refresh}"

    def __repr__(self) -> str:
        return self.email


def post_form(url: str, data: dict) -> dict:
    """POST application/x-www-form-urlencoded，返回解析后的 JSON。"""
    body = urllib.parse.urlencode(data).encode("utf-8")
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        try:
            return json.loads(raw)
        except Exception:
            return {"error": str(exc.code), "error_description": raw[:500]}


def try_refresh(acc: Account, client_id: str) -> tuple[bool, str, dict]:
    """尝试用指定 client_id 刷新 refresh_token。返回 (ok, new_refresh, resp)。"""
    if not acc.refresh_token:
        return False, "", {"error": "no_token"}
    resp = post_form(TOKEN_URL, {
        "client_id": client_id,
        "grant_type": "refresh_token",
        "refresh_token": acc.refresh_token,
        "scope": SCOPE,
    })
    if "access_token" in resp:
        return True, resp.get("refresh_token", acc.refresh_token), resp
    return False, "", resp


def device_code_flow(acc: Account, client_id: str, json_out: Path | None = None) -> tuple[bool, str]:
    """Device Code Flow 交互授权。返回 (ok, refresh_token)。
    传入 json_out 时把 device_code 信息写入文件（供外部浏览器自动化使用）。
    """
    resp = post_form(DEVICE_CODE_URL, {
        "client_id": client_id,
        "scope": SCOPE,
    })
    if "device_code" not in resp:
        print(f"    [错误] 获取 device_code 失败: {resp.get('error')} {resp.get('error_description', '')[:200]}")
        return False, ""
    device_code = resp["device_code"]
    user_code = resp["user_code"]
    verify_url = resp.get("verification_uri", "https://microsoft.com/devicelogin")
    expires_in = resp.get("expires_in", 900)
    interval = resp.get("interval", 5)

    print(f"    ┌─ 请在浏览器打开: {verify_url}")
    print(f"    ├─ 输入代码: {user_code}")
    print(f"    ├─ 登录账号: {acc.email} 并同意授权")
    print(f"    └─ 等待授权中…（{expires_in} 秒内有效，Ctrl+C 跳过该账号）")

    if json_out is not None:
        json_out.write_text(json.dumps({
            "email": acc.email,
            "password": acc.password,
            "user_code": user_code,
            "verification_uri": verify_url,
            "device_code": device_code,
            "client_id": client_id,
            "expires_in": expires_in,
        }, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"    [外部自动化] device 信息已写入 {json_out}")
    # 轮询拿 token
    deadline = time.time() + expires_in
    while time.time() < deadline:
        time.sleep(interval)
        poll = post_form(TOKEN_URL, {
            "client_id": client_id,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "device_code": device_code,
        })
        if "access_token" in poll:
            return True, poll.get("refresh_token", "")
        err = poll.get("error", "")
        if err == "authorization_pending":
            continue
        if err in ("authorization_declined", "expired_token"):
            print(f"    [结果] 授权未完成/已过期: {err}")
            return False, ""
        print(f"    [结果] 轮询异常: {err}")
        return False, ""
    print("    [结果] 授权超时")
    return False, ""


def load_accounts(path: Path) -> list[Account]:
    """解析 secured_tokens.txt: email---password---refresh_token---access_token---expire_at"""
    accounts: list[Account] = []
    for n, raw in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("---")
        if len(parts) < 3:
            print(f"  [跳过] 第 {n} 行格式无效: {line[:60]}")
            continue
        email = parts[0].strip().lower()
        password = parts[1].strip()
        refresh_token = parts[2].strip()
        access_token = parts[3].strip() if len(parts) > 3 else ""
        expire_at = parts[4].strip() if len(parts) > 4 else ""
        accounts.append(Account(email, password, refresh_token, access_token, expire_at))
    return accounts


def main() -> None:
    parser = argparse.ArgumentParser(description="补齐 client_id 并刷新 Outlook refresh_token")
    parser.add_argument("--source", required=True, help="secured_tokens.txt 路径")
    parser.add_argument("--interactive", action="store_true", help="刷新失败时进入交互授权")
    parser.add_argument("--client-id", default=THUNDERBIRD_CLIENT_ID, help="用于刷新的 client_id")
    parser.add_argument("--limit", type=int, default=0, help="只处理前 N 个账号（0=全部）")
    parser.add_argument("--device-json", help="把第一个 device_code 信息写入该 JSON 文件后退出（供外部自动化）")
    args = parser.parse_args()

    src = Path(args.source)
    if not src.exists():
        print(f"文件不存在: {src}")
        sys.exit(1)

    accounts = load_accounts(src)
    if args.limit > 0:
        accounts = accounts[:args.limit]
    print(f"共读取 {len(accounts)} 个账号\n")

    # 仅输出 device 信息给外部自动化（Playwright），获取后脚本继续轮询等待
    if args.device_json:
        acc = accounts[0]
        device_json = Path(args.device_json)
        print(f"== 获取账号 1 的 Device Code ==")
        ok, new_refresh = device_code_flow(acc, args.client_id, json_out=device_json)
        if ok:
            result = device_json.with_name(device_json.stem + "_result.txt")
            result.write_text(acc.to_mailpool(args.client_id, new_refresh), encoding="utf-8")
            print(f"  ✓ 授权成功，已写入 {result}")
        else:
            print("  ✗ 授权失败/超时")
        print("脚本结束")
        return

    ok_lines: list[str] = []
    fail_list: list[Account] = []
    client_id = args.client_id

    print("== 第 1 步: 自动刷新现有 refresh_token ==")
    for i, acc in enumerate(accounts, 1):
        ok, new_refresh, resp = try_refresh(acc, client_id)
        if ok:
            ok_lines.append(acc.to_mailpool(client_id, new_refresh))
            print(f"  [{i}/{len(accounts)}] {acc.email}  ✓ 刷新成功 (新token长度 {len(new_refresh)})")
        else:
            err = resp.get("error", "?")
            print(f"  [{i}/{len(accounts)}] {acc.email}  ✗ {err}")
            fail_list.append(acc)
        time.sleep(0.5)

    print(f"\n自动刷新成功: {len(ok_lines)} 个, 失败: {len(fail_list)} 个")

    # 交互授权失败账号
    if fail_list and args.interactive:
        print("\n== 第 2 步: Device Code Flow 交互授权 ==")
        for i, acc in enumerate(fail_list, 1):
            print(f"\n  [{i}/{len(fail_list)}] {acc.email}")
            ok, new_refresh = device_code_flow(acc, client_id)
            if ok:
                ok_lines.append(acc.to_mailpool(client_id, new_refresh))
                print(f"  ✓ 授权成功")
            time.sleep(1)

    # 输出 MailPool 导入文件
    out = src.with_name("secured_tokens_mailpool.txt")
    out.write_text("\n".join(ok_lines) + ("\n" if ok_lines else ""), encoding="utf-8")
    print(f"\n结果: 共 {len(ok_lines)} 个可用账号 → {out}")
    if fail_list:
        print(f"仍有 {len(fail_list)} 个账号未补齐（可用 --interactive 交互授权）: ")
        for acc in fail_list:
            print(f"    - {acc.email}")


if __name__ == "__main__":
    main()
