#!/usr/bin/env python3
"""最小 S3 兼容 mock 服务器，用于验证 MailPool 备份上传路径。

只实现备份功能用到的那几个操作：
- HEAD /{bucket}              -> 200（head_bucket，测试连接）
- PUT  /{bucket}/{key...}     -> 200（put_object，上传）
- GET  /{bucket}/{bucket}/{k} -> head_bucket 的 path-style 变体
- DELETE /{bucket}/{key...}   -> 204（delete_object，清理旧备份）

**不验证签名**（我们要测的是"客户端能不能正确发出请求并处理响应"，
签名算法由 aws-sdk 保证，这里验证反而是重复劳动）。

用法：
    python s3_mock.py 19000
"""
import base64
import hashlib
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

STORAGE = os.environ.get("S3_MOCK_DIR", "./_s3_mock_storage")
os.makedirs(STORAGE, exist_ok=True)

# 记录收到的请求，便于断言
LOG = []
LOG_LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass  # 静音默认日志

    # ── 工具 ──
    def _parts(self):
        path = unquote(self.path.split("?")[0]).lstrip("/")
        return path.split("/", 1) if "/" in path else (path, "")

    def _record(self, method, status, note=""):
        with LOG_LOCK:
            LOG.append({
                "method": method,
                "path": self.path,
                "status": status,
                "note": note,
                "len": self.headers.get("Content-Length", "0"),
            })

    def _reply(self, code, body=b"", headers=None):
        self.send_response(code)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body and self.command != "HEAD":
            self.wfile.write(body)

    # ── HEAD：head_bucket / head_object ──
    def do_HEAD(self):
        bucket, key = self._parts()
        if not key:
            # head_bucket：桶存在就 200
            self._record("HEAD", 200, f"head_bucket({bucket})")
            self._reply(200, headers={"x-amz-bucket-region": "us-east-1"})
        else:
            p = os.path.join(STORAGE, bucket, key)
            if os.path.exists(p):
                size = os.path.getsize(p)
                self._record("HEAD", 200, f"head_object({key})")
                self._reply(200, headers={"Content-Length": str(size),
                                          "Content-Type": "application/gzip"})
            else:
                self._record("HEAD", 404, f"head_object miss({key})")
                self._reply(404)

    # ── PUT：put_object ──
    def do_PUT(self):
        bucket, key = self._parts()
        n = int(self.headers.get("Content-Length", "0"))
        data = self.rfile.read(n) if n else b""

        dest = os.path.join(STORAGE, bucket, key)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, "wb") as f:
            f.write(data)

        sha = hashlib.sha256(data).hexdigest()
        self._record("PUT", 200, f"put_object({key}) {len(data)}B sha={sha[:16]}")
        self._reply(200, headers={"ETag": f'"{sha[:32]}"'})

    # ── GET：list_objects_v2（清理时可能用）+ 下载 ──
    def do_GET(self):
        bucket, key = self._parts()
        # list_objects_v2: /{bucket}?list-type=2
        if not key and "list-type=2" in self.path:
            items = []
            bdir = os.path.join(STORAGE, bucket)
            if os.path.isdir(bdir):
                for root, _, files in os.walk(bdir):
                    for fn in files:
                        full = os.path.join(root, fn)
                        rel = os.path.relpath(full, bdir).replace(os.sep, "/")
                        items.append((rel, os.path.getsize(full)))
            xml = ['<?xml version="1.0" encoding="UTF-8"?>',
                   '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
                   f'<Name>{bucket}</Name><KeyCount>{len(items)}</KeyCount>',
                   '<IsTruncated>false</IsTruncated>']
            for k, size in items:
                xml.append(f'<Contents><Key>{k}</Key><Size>{size}</Size>'
                           f'<LastModified>2026-11-22T00:00:00.000Z</LastModified>'
                           f'<ETag>"etag"</ETag><StorageClass>STANDARD</StorageClass></Contents>')
            xml.append('</ListBucketResult>')
            body = "".join(xml).encode()
            self._record("GET", 200, f"list_objects_v2({bucket}) {len(items)} items")
            self._reply(200, body=body, headers={"Content-Type": "application/xml"})
            return

        p = os.path.join(STORAGE, bucket, key) if key else None
        if p and os.path.exists(p):
            with open(p, "rb") as f:
                data = f.read()
            self._record("GET", 200, f"get_object({key}) {len(data)}B")
            self._reply(200, body=data, headers={"Content-Type": "application/gzip"})
        else:
            self._record("GET", 404, f"miss({key})")
            self._reply(404)

    # ── DELETE：delete_object ──
    def do_DELETE(self):
        bucket, key = self._parts()
        p = os.path.join(STORAGE, bucket, key)
        if os.path.exists(p):
            os.remove(p)
            self._record("DELETE", 204, f"delete_object({key})")
        else:
            self._record("DELETE", 204, f"delete_object miss({key})")
        self._reply(204)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 19000
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"S3 mock listening on http://127.0.0.1:{port}", flush=True)
    print(f"storage: {os.path.abspath(STORAGE)}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
