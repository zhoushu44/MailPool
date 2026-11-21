# 贡献指南

## 开发环境

```bash
git clone https://github.com/zhoushu44/MailPool.git
cd MailPool
python -m venv .venv
source .venv/bin/activate        # Windows: .venv\Scripts\activate
pip install -r requirements.txt

export MAILPOOL_ADMIN_PASSWORD=dev-password
export MAILPOOL_DATA=./data
python -m app.main
```

打开 http://127.0.0.1:8000 ，用 `dev-password` 登录。

## 跑测试

```bash
python -m pytest tests/ -q
```

测试会把数据库隔离到临时目录（`tests/conftest.py` 里设 `MAILPOOL_DATA`），
不会污染生产库。**不要**在测试里指向真实数据库。

## 代码风格

- Python 3.11+，用 `from __future__ import annotations`
- 类型标注尽量写全
- 中文注释与日志：这个项目的运维现场是中文，日志直接用中文
- 提交信息用 `type(scope): 描述` 格式，例如 `fix(pool): 死号不再发出去`

## 提交前检查

1. `python -m pytest tests/ -q` 全绿
2. `python -c "import app.main"` 无导入错误
3. **不要提交任何凭据**：`data/`、`.env`、`*.pem`、`*.key`、`refresh_token`、服务器地址
4. 新增/修改的公开 API 行为要同步更新 `docs/api-overview.html` 与 README

## 新增邮箱 Provider

1. 继承 `app/providers/base.py` 的 `EmailProvider`
2. 用 `@register` 装饰器注册
3. 实现 `generate_address()`、`wait_code()`；需要完整邮件的实现 `wait_mail()`
4. 覆写 `_fallback_extract_from_text()` 如果验证码格式特殊
5. 在 `app/providers/__init__.py` 里 import，让注册表加载到

## 报告问题

- 功能缺陷、使用问题：开 Issue
- 安全漏洞：见 [SECURITY.md](SECURITY.md)，**不要**开公开 Issue
