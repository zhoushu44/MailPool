#!/usr/bin/env python3
"""差分测试：Python 版 vs Rust 版 逐接口行为比对。

用法：
    python rust/scripts/diff_test.py \
        --python http://127.0.0.1:18090 \
        --rust   http://127.0.0.1:18080 \
        --key    ak_xxxx

比对规则（实现见 rust/src/testing.rs）：
- Missing / TypeMismatch  → 阻塞，必须修
- ValueMismatch           → 报告（可能是随机量：订单号、时间戳）
- Formatting              → 时间戳格式差异，可接受

退出码：0 = 无阻塞差异；1 = 有阻塞差异。
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from typing import Any


# ── 请求 ────────────────────────────────────────────────────────────
def http_get(base: str, path: str, key: str | None = None, timeout: float = 60) -> tuple[int, Any]:
    req = urllib.request.Request(base.rstrip("/") + path)
    if key:
        req.add_header("Authorization", f"Bearer {key}")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read().decode("utf-8", errors="replace")
            try:
                return r.status, json.loads(body)
            except json.JSONDecodeError:
                return r.status, body
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(body)
        except json.JSONDecodeError:
            return e.code, body
    except Exception as e:  # noqa: BLE001
        return -1, f"{type(e).__name__}: {e}"


def http_post(base: str, path: str, payload: dict, key: str | None = None, timeout: float = 300) -> tuple[int, Any]:
    data = json.dumps(payload).encode()
    req = urllib.request.Request(base.rstrip("/") + path, data=data, method="POST")
    req.add_header("Content-Type", "application/json")
    if key:
        req.add_header("Authorization", f"Bearer {key}")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            body = r.read().decode("utf-8", errors="replace")
            try:
                return r.status, json.loads(body)
            except json.JSONDecodeError:
                return r.status, body
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(body)
        except json.JSONDecodeError:
            return e.code, body
    except Exception as e:  # noqa: BLE001
        return -1, f"{type(e).__name__}: {e}"


# ── 比对 ────────────────────────────────────────────────────────────
TYPE_NAMES = {
    dict: "object",
    list: "array",
    str: "string",
    bool: "bool",
    int: "number",
    float: "number",
    type(None): "null",
}


def type_name(v: Any) -> str:
    # bool 是 int 的子类，必须先判
    if isinstance(v, bool):
        return "bool"
    return TYPE_NAMES.get(type(v), type(v).__name__)


def join(path: str, key: str) -> str:
    return f"{path}.{key}" if path else key


def is_time_field(path: str) -> bool:
    last = path.rsplit(".", 1)[-1]
    return last.endswith("_at") or last.endswith("_time")


def looks_time(s: str) -> bool:
    return len(s) >= 19 and s[4:5] == "-" and ("T" in s or " " in s)


class Diff:
    def __init__(self, path: str, py: Any, rs: Any, kind: str):
        self.path, self.py, self.rs, self.kind = path, py, rs, kind

    def blocking(self) -> bool:
        return self.kind in ("missing", "type")

    def __repr__(self) -> str:
        return f"[{self.kind}] {self.path}: python={self.py!r} rust={self.rs!r}"


def compare(py: Any, rs: Any, path: str = "", out: list[Diff] | None = None) -> list[Diff]:
    if out is None:
        out = []
    tp, tr = type_name(py), type_name(rs)
    if tp != tr:
        out.append(Diff(path, f"{tp}:{py}", f"{tr}:{rs}", "type"))
        return out

    if isinstance(py, dict):
        for k in py:
            if k not in rs:
                out.append(Diff(join(path, k), "存在", "缺失", "missing"))
        for k in rs:
            if k not in py:
                out.append(Diff(join(path, k), "缺失", "存在", "missing"))
        for k in py:
            if k in rs:
                compare(py[k], rs[k], join(path, k), out)
    elif isinstance(py, list):
        if len(py) != len(rs):
            out.append(Diff(path, f"{len(py)}项", f"{len(rs)}项", "value"))
        for i, (a, b) in enumerate(zip(py, rs)):
            compare(a, b, f"{path}[{i}]", out)
    elif isinstance(py, str):
        if py != rs:
            if is_time_field(path) and looks_time(py) and looks_time(rs):
                out.append(Diff(path, py, rs, "formatting"))
            else:
                out.append(Diff(path, py, rs, "value"))
    elif isinstance(py, (int, float)) and not isinstance(py, bool):
        if py != rs:
            out.append(Diff(path, py, rs, "value"))
    elif isinstance(py, bool):
        if py != rs:
            out.append(Diff(path, py, rs, "value"))
    return out


# ── 用例 ────────────────────────────────────────────────────────────
def build_cases(key: str | None) -> list[dict]:
    """返回 [{name, method, path, payload}]。

    只放**不产生副作用**的用例，差分测试可以反复跑。
    """
    cases = [
        # 公开接口（无鉴权）
        {"name": "product_info", "method": "GET", "path": "/api/product/info"},
        {"name": "api_index", "method": "GET", "path": "/api"},

        # 需鉴权
        {"name": "health", "method": "GET", "path": "/api/v1/health", "auth": True},
        {"name": "mails", "method": "GET", "path": "/api/v1/mails?page=1&page_size=5", "auth": True},
        {"name": "balance", "method": "GET", "path": "/api/v1/recharge", "auth": True},
        {"name": "channels", "method": "GET", "path": "/api/v1/recharge/channels", "auth": True},
        {"name": "recharge_orders", "method": "GET", "path": "/api/v1/recharge/orders?page=1", "auth": True},

        # 管理接口
        {"name": "admin_summary", "method": "GET", "path": "/api/admin/overview/summary", "auth": True},
        {"name": "admin_projects", "method": "GET", "path": "/api/admin/projects", "auth": True},
        {"name": "admin_emails", "method": "GET", "path": "/api/admin/emails?limit=10&offset=0", "auth": True},
        {"name": "admin_tags", "method": "GET", "path": "/api/admin/emails/tags", "auth": True},
        {"name": "admin_codes", "method": "GET", "path": "/api/admin/codes?page=1&page_size=5", "auth": True},
        {"name": "admin_health_summary", "method": "GET", "path": "/api/admin/pool/health-summary", "auth": True},
        {"name": "admin_cid_stats", "method": "GET", "path": "/api/admin/pool/client-id-stats", "auth": True},
        {"name": "admin_providers", "method": "GET", "path": "/api/admin/providers", "auth": True},
        {"name": "admin_api_keys", "method": "GET", "path": "/api/admin/api-keys", "auth": True},
        {"name": "admin_billing_users", "method": "GET", "path": "/api/admin/billing/users?page=1&page_size=5", "auth": True},
        {"name": "admin_billing_config", "method": "GET", "path": "/api/admin/billing/config", "auth": True},
        {"name": "admin_billing_orders", "method": "GET", "path": "/api/admin/billing/orders?page=1&page_size=5", "auth": True},
        {"name": "admin_health_cfg", "method": "GET", "path": "/api/admin/pool/health-check-config", "auth": True},
        {"name": "admin_heartbeat_cfg", "method": "GET", "path": "/api/admin/pool/heartbeat-status", "auth": True},
        {"name": "admin_refresh_cfg", "method": "GET", "path": "/api/admin/pool/refresh-config", "auth": True},

        # 错误路径（不该 500）
        {"name": "unknown_project", "method": "POST", "path": "/api/v1/email/temporary/buy",
         "payload": {"project_code": "__nonexistent__", "count": 1}, "auth": True},
        {"name": "bad_count", "method": "POST", "path": "/api/v1/email/temporary/buy",
         "payload": {"project_code": "chatgpt001", "count": 999}, "auth": True},
    ]
    return cases


def main() -> int:
    ap = argparse.ArgumentParser(description="MailPool Python vs Rust 差分测试")
    ap.add_argument("--python", required=True, help="Python 版地址")
    ap.add_argument("--rust", required=True, help="Rust 版地址")
    ap.add_argument("--key", default=None, help="API Key")
    ap.add_argument("--verbose", "-v", action="store_true")
    args = ap.parse_args()

    cases = build_cases(args.key)
    total_blocking = 0
    total_value = 0
    failed_cases = 0

    print(f"{'用例':<26} {'阻塞':>4} {'值不同':>6} {'状态':<10}")
    print("-" * 60)

    for c in cases:
        key = args.key if c.get("auth") else None
        if c["method"] == "GET":
            py_status, py_body = http_get(args.python, c["path"], key)
            rs_status, rs_body = http_get(args.rust, c["path"], key)
        else:
            py_status, py_body = http_post(args.python, c["path"], c.get("payload", {}), key)
            rs_status, rs_body = http_post(args.rust, c["path"], c.get("payload", {}), key)

        status_note = ""
        if py_status != rs_status:
            status_note = f"HTTP {py_status}/{rs_status}"
            total_blocking += 1

        diffs = compare(py_body, rs_body, "") if isinstance(py_body, (dict, list)) and isinstance(rs_body, (dict, list)) else []
        blocking = [d for d in diffs if d.blocking()]
        values = [d for d in diffs if d.kind == "value"]

        total_blocking += len(blocking)
        total_value += len(values)
        if blocking or status_note:
            failed_cases += 1

        mark = "✅" if not blocking and not status_note else "❌"
        print(f"{mark} {c['name']:<24} {len(blocking):>4} {len(values):>6} {status_note:<10}")

        if args.verbose or blocking or status_note:
            for d in blocking:
                print(f"      {d}")
            if status_note:
                print(f"      HTTP 状态不一致: python={py_status} rust={rs_status}")
                print(f"      python body: {str(py_body)[:200]}")
                print(f"      rust   body: {str(rs_body)[:200]}")

    print("-" * 60)
    print(f"用例 {len(cases)} 个，失败 {failed_cases} 个")
    print(f"阻塞差异 {total_blocking} 项，值差异 {total_value} 项")

    if total_blocking == 0:
        print("\n✅ 无阻塞差异，可以切换")
        return 0
    print("\n❌ 存在阻塞差异，请修复后再切换")
    return 1


if __name__ == "__main__":
    sys.exit(main())
