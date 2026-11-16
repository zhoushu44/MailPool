"""MailPool —— 自托管多源邮箱接收平台。

仿 MailNest 的 project_code 取号/取码模型，但邮箱源完全自主：
支持 CF Worker 临时邮箱、MailNest 临时邮箱、TempMail.lol 临时邮箱、
私有 Outlook 邮箱池（Graph + IMAP），未来可扩展 iCloud 等。
"""
