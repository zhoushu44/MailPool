"""全局配置：路径 + 环境变量。"""
from __future__ import annotations

import os
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
DATA_DIR = Path(os.getenv("MAILPOOL_DATA", str(BASE_DIR / "data")))
DB_PATH = DATA_DIR / "mailpool.db"
STATIC_DIR = BASE_DIR / "app" / "web" / "static"
TEMPLATE_DIR = BASE_DIR / "app" / "web" / "templates"


class Settings:
    def __init__(self) -> None:
        self.host: str = os.getenv("MAILPOOL_HOST", "0.0.0.0")
        self.port: int = int(os.getenv("MAILPOOL_PORT", "18080"))
        # 管理后台的访问口令（若为空则后台免密，仅本机友好提示）
        self.admin_password: str = os.getenv("MAILPOOL_ADMIN_PASSWORD", "")
        # 默认收码超时（秒）
        self.default_timeout: int = int(os.getenv("MAILPOOL_TIMEOUT", "120"))
        self.data_dir = DATA_DIR


settings = Settings()


# ── 内置项目 Logo 库（创建/编辑项目时可选，png 存放于 static/img/projects/，来源：迈巢 mailnest.top 项目图标）──
PROJECT_LOGOS: list[dict[str, str]] = [
    {"id": "kick", "name": "Kick"},
    {"id": "opensea", "name": "OpenSea"},
    {"id": "z_ai", "name": "Z.AI"},
    {"id": "xai", "name": "xAI"},
    {"id": "microsoft", "name": "Microsoft 系通用"},
    {"id": "google", "name": "Google 系通用"},
    {"id": "apple", "name": "Apple 系通用"},
    {"id": "claude", "name": "Claude"},
    {"id": "chatgpt", "name": "ChatGPT"},
    {"id": "cursor", "name": "Cursor"},
    {"id": "ollama", "name": "Ollama"},
    {"id": "huggingface", "name": "HuggingFace"},
    {"id": "rubii", "name": "Rubii"},
    {"id": "mulerun", "name": "MuleRun"},
    {"id": "klingai", "name": "KlingAI"},
    {"id": "miraimind", "name": "MiraiMind"},
    {"id": "warp", "name": "Warp"},
    {"id": "aihubmix", "name": "AIHubMix"},
    {"id": "windsurf", "name": "Devin / Windsurf"},
    {"id": "dreamina", "name": "Dreamina"},
    {"id": "manus", "name": "Manus"},
    {"id": "pixmax", "name": "PixMax"},
    {"id": "stagewise", "name": "StageWise"},
    {"id": "studentai", "name": "StudentAI"},
    {"id": "traq", "name": "Traq"},
    {"id": "weryai", "name": "WeryAI"},
    {"id": "zopia", "name": "Zopia"},
    {"id": "linkedin", "name": "LinkedIn 领英"},
    {"id": "teamviewer", "name": "TeamViewer"},
    {"id": "zoom", "name": "Zoom"},
    {"id": "x", "name": "X 推特"},
    {"id": "discord", "name": "Discord"},
    {"id": "facebook", "name": "Facebook"},
    {"id": "reddit", "name": "Reddit"},
    {"id": "netflix", "name": "Netflix"},
    {"id": "amazon", "name": "Amazon"},
    {"id": "shopify", "name": "Shopify"},
    {"id": "temu", "name": "Temu"},
    {"id": "shein", "name": "SHEIN"},
    {"id": "github", "name": "GitHub"},
    {"id": "gitlab", "name": "GitLab"},
    {"id": "cloudflare", "name": "Cloudflare"},
    {"id": "nvidia", "name": "NVIDIA"},
    {"id": "resend", "name": "Resend"},
    {"id": "aws", "name": "AWS Builder ID / Kiro"},
    {"id": "jetbrains", "name": "JetBrains"},
    {"id": "binance", "name": "币安"},
    {"id": "dune", "name": "Dune"},
    {"id": "bitpay", "name": "BitPay"},
    {"id": "ic", "name": "IC"},
    {"id": "fileyourtaxes", "name": "FileYourTaxes"},
]


def project_logo_src(logo: str | None) -> str:
    """项目 config 里的 logo 字段 → 图片地址。

    - 内置 id（如 chatgpt）→ /static/img/projects/chatgpt.png（找不到则回退 .svg）
    - 以 http 开头 → 视为自定义图片 URL 原样返回
    - 空 → 返回空（前端用首字母占位）
    """
    logo = (logo or "").strip()
    if not logo:
        return ""
    if logo.startswith(("http://", "https://")):
        return logo
    base = logo.split("/")[-1]
    base = base.rsplit(".", 1)[0] if base.lower().endswith((".png", ".svg", ".jpg", ".jpeg")) else base
    for ext in (".png", ".svg"):
        if (STATIC_DIR / "img" / "projects" / f"{base}{ext}").exists():
            return f"/static/img/projects/{base}{ext}"
    return f"/static/img/projects/{base}.png"

