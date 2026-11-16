# -*- coding: utf-8 -*-
"""MailPool 全面 API 测试：新功能端点 + 旧端点回归。"""
import json, sys, time
import urllib.request, urllib.error

BASE = "http://127.0.0.1:18080"
PASS, FAIL = 0, 0
FAILED = []

def call(method, path, body=None, raw=False):
    url = BASE + path
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            txt = resp.read().decode()
            if raw:
                return resp.status, txt
            try:
                return resp.status, json.loads(txt)
            except Exception:
                return resp.status, {"raw": txt[:300]}
    except urllib.error.HTTPError as e:
        try:
            txt = e.read().decode()
        except Exception:
            txt = ""
        try:
            return e.code, json.loads(txt)
        except Exception:
            return e.code, {"raw": txt[:300]}
    except Exception as e:
        return 0, {"error": str(e)}

def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        FAILED.append(name)
        print(f"  FAIL  {name}  -> {detail}")

# ── 1. 账号列表（含新字段 tags/proxy）────────────────
print("\n[1] 账号列表")
s, d = call("GET", "/api/admin/emails?provider=outlook&limit=5")
check("emails 状态码 200", s == 200, s)
accs = d.get("data", {}).get("accounts", [])
check("emails 返回账号", isinstance(accs, list) and len(accs) > 0, d)
if accs:
    a0 = accs[0]
    check("含 tags 字段", "tags" in a0)
    check("含 proxy 字段", "proxy" in a0)
    check("含 remark 字段", "remark" in a0)

# ── 2. 标签相关 ─────────────────────────────────────
print("\n[2] 标签管理")
s, d = call("GET", "/api/admin/emails/tags?provider=outlook")
check("tags 列表 200", s == 200, s)
check("tags 是 list", isinstance(d.get("data"), list), d)

# 更新标签/代理/备注（用第一个真实账号 id）
real_id = accs[0]["id"]
s, d = call("PATCH", f"/api/admin/emails/{real_id}",
            {"tags": "全面测试标签", "proxy": "http://127.0.0.1:9999", "remark": "autotest"})
check("PATCH 更新 200", s == 200, (s, d))
if s == 200:
    check("PATCH 返回 tags", d.get("data", {}).get("tags") == "全面测试标签", d)
    check("PATCH 返回 proxy", d.get("data", {}).get("proxy") == "http://127.0.0.1:9999", d)
# 无效账号 PATCH
s, d = call("PATCH", "/api/admin/emails/99999", {"tags": "x"})
check("PATCH 无效账号 404", s == 404, s)

# ── 3. 导出 ─────────────────────────────────────────
print("\n[3] 导出账号")
s, d = call("GET", "/api/admin/emails/export?provider=outlook&limit=3")
check("export 200", s == 200, s)
data = d.get("data", {})
check("export lines 存在", isinstance(data.get("lines"), list), d)
check("export count 正确", data.get("count") == len(data.get("lines", [])), d)
if data.get("lines"):
    line = data["lines"][0]
    check("export 格式 4 段", len(line.split("----")) == 4, line)
    check("export 行含 email", line.split("----")[0].endswith("@outlook.com"), line)

# ── 4. 验证码日志 ───────────────────────────────────
print("\n[4] 验证码提取日志")
s, d = call("GET", "/api/admin/codes?limit=10")
check("codes 200", s == 200, s)
check("codes data.records", isinstance(d.get("data", {}).get("records"), list), d)
check("codes 有 total", "total" in d.get("data", {}), d)

# ── 5. 刷新历史 ─────────────────────────────────────
print("\n[5] 刷新历史")
s, d = call("GET", "/api/admin/emails/refresh-logs?limit=10")
check("refresh-logs 200", s == 200, s)
logs = d.get("data", {}).get("logs", [])
check("refresh-logs 有记录", isinstance(logs, list), d)
if logs:
    l0 = logs[0]
    for f in ("trigger", "status", "total", "success", "failed", "skipped", "duration"):
        check(f"log 含字段 {f}", f in l0, l0)

# ── 6. 定时刷新配置 ─────────────────────────────────
print("\n[6] 定时刷新配置")
s, d = call("GET", "/api/admin/pool/refresh-config")
check("get refresh-config 200", s == 200, s)
cfg = d.get("data", {}).get("config", {})
check("config 含 refresh_enabled", "refresh_enabled" in cfg, cfg)
s, d = call("PUT", "/api/admin/pool/refresh-config",
            {"refresh_enabled": "false", "refresh_hour": "4", "refresh_minute": "30"})
check("put refresh-config 200", s == 200, s)

# ── 7. 概览看板 ─────────────────────────────────────
print("\n[7] 概览大盘")
s, d = call("GET", "/api/admin/overview/summary")
check("summary 200", s == 200, s)
ddata = d.get("data", {})
for key in ("accounts", "pool", "today", "projects", "monitoring"):
    check(f"summary 含 {key}", key in ddata, list(ddata.keys()))
s, d = call("GET", "/api/admin/overview/refreshes")
check("refreshes 200", s == 200, s)
rd = d.get("data", {})
for key in ("last", "today_count", "history_count", "config", "last_run"):
    check(f"refreshes 含 {key}", key in rd, list(rd.keys()))

# ── 8. 旧端点回归（取号/收信/释放/列表等）────────────
print("\n[8] 旧端点回归")
s, d = call("GET", "/api/admin/projects")
check("projects 列表 200", s == 200, s)
s, d = call("POST", "/api/email/temporary/buy", {"project_code": "__not_exist__", "count": 1})
check("公开取号(不存在项目) 非 500", s != 500, (s, d))
if s == 404:
    check("公开取号 404 合理", True)
s, d = call("POST", "/api/email/temporary/buy", {"project_code": "__not_exist__"})
check("公开取号参数校验不崩", s != 500, (s, d))
s, d = call("GET", "/api/admin/emails/refresh-all/status?task_id=nonexist")
check("refresh-all/status 不存在任务", s == 200 and d.get("data", {}).get("status") == "not_found", d)

# ── 9. 全量刷新启动（幂等/并发保护）─────────────────
print("\n[9] 全量刷新")
s, d = call("POST", "/api/admin/emails/refresh-all", {"provider": "outlook"})
check("refresh-all 200", s == 200, (s, d))
s2, d2 = call("POST", "/api/admin/emails/refresh-all", {"provider": "outlook"})
check("重复发起拒绝或幂等", s2 == 200, (s2, d2))
# 等待完成，验证历史落库
time.sleep(10)
s3, d3 = call("GET", "/api/admin/emails/refresh-logs?limit=1")
logs3 = d3.get("data", {}).get("logs", [])
check("刷新后历史有记录", len(logs3) >= 1, d3)
if logs3:
    check("历史 status=finished", logs3[0].get("status") == "finished", logs3[0])

print(f"\n{'='*50}\n结果: PASS {PASS} / FAIL {FAIL}")
if FAILED:
    print("失败项:", FAILED)
    sys.exit(1)
print("全部通过")