#!/bin/bash
# 确认 CI 推的镜像在哪个仓库，并拉下来
set -u
for repo in zhoushu1/mailpool zhoushu44/mailpool; do
  echo "=== $repo ==="
  if docker pull "${repo}:1.0" 2>&1 | tail -3; then
    ID=$(docker image inspect "${repo}:1.0" --format '{{.Id}}' 2>/dev/null)
    CREATED=$(docker image inspect "${repo}:1.0" --format '{{.Created}}' 2>/dev/null)
    echo "    ID=${ID:0:19}  Created=$CREATED"
  fi
done
echo
echo "=== 当前容器用的镜像 ==="
docker inspect mailpool --format '{{.Image}}'
docker image inspect zhoushu1/mailpool:1.0 --format '{{.Id}}'
