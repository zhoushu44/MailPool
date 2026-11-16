"""Provider 包：所有邮箱源。"""
from .base import (
    ConfigField,
    EmailProvider,
    ProviderError,
    create_provider,
    extract_code,
    extract_code_from_mail,
    get_provider_class,
    list_providers,
    register,
)

__all__ = [
    "EmailProvider",
    "ProviderError",
    "ConfigField",
    "register",
    "get_provider_class",
    "create_provider",
    "list_providers",
    "extract_code",
    "extract_code_from_mail",
]
