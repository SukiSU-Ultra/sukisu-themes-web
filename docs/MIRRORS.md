# 镜像选择与 302 跳转

服务端仍然是权威主题源。配置镜像后，首次主题包下载会由服务端选择健康镜像并返回 `302`；管理器（以及浏览器）只是普通 HTTP 客户端，会自动跟随跳转，不需要安装任何 P2P/WebRTC 库。

## 配置

默认读取 `/etc/sukisu-theme-web/mirrors.json`，可用 `THEME_MIRRORS_FILE` 覆盖。文件每 30 秒自动重载，修改后不需要重启服务。

```json
{
  "mirrors": [
    {
      "name": "edge-a",
      "baseUrl": "https://a.example.com/",
      "token": "",
      "priority": 10,
      "enabled": true
    },
    {
      "name": "edge-b",
      "baseUrl": "https://b.example.com/",
      "token": "",
      "priority": 20,
      "enabled": true
    }
  ]
}
```

- `baseUrl`：公网可达的 HTTP/HTTPS 根地址；镜像必须提供与本服务相同的 `themes/<id>-v<version>.sukisu` 路径。
- `priority`：数字越小越优先。
- `enabled`：`false` 时跳过。
- `token`：仅用于服务端探测镜像时发送 `X-Sukisu-Theme-Token`；不会写进 302 地址。管理器跟随跳转时会转发自己的 token，因此镜像要么接受同一个 token，要么公开可读。
- 最多 16 个镜像；无效 URL、带 query/fragment 的地址会被忽略。

## 选择与回退

1. 只处理 `themes/...` 且请求头**没有 `Range`** 的下载。
2. 按 `priority` 顺序对同一路径做 `HEAD`；同优先级的镜像轮询选择，避免所有下载压到同一台；镜像不支持 `HEAD` 时改用 `Range: bytes=0-0` 的 GET。
3. 第一个探测成功的镜像返回 `302 Location: <baseUrl>/<path>?sukisu_mirror=1`。
4. 成功结果缓存 60 秒，失败缓存 20 秒；全部候选失败或总探测超过 4 秒，则由本服务器直接提供文件。
5. 带 `Range` 的断点续传始终由本服务器处理，避免不同源的字节混在一个临时文件里。
6. `sukisu_mirror=1` 是自指保护标记：如果镜像误配成本服务器，第二次请求会直接本地服务，不会循环跳转。
7. 302 发出时计数一次；自指回退不会再计数。

## 镜像要求

- 与本服务器目录结构一致，文件内容字节一致（管理器仍会校验 SHA-256）。
- 支持 `HEAD`，或至少支持 `Range: bytes=0-0`。
- 能通过公网 HTTP/HTTPS 访问；不支持 AList/网盘的私有 API 地址。
- 如果镜像使用 Caddy token 门禁，必须接受管理器转发过来的同一个 `X-Sukisu-Theme-Token`；否则请把镜像设为公开可读。

## 验证

```bash
TOKEN='...'
BASE='https://themes.example.com:8443'

# 有健康镜像时应返回 302
curl -sI -H "X-Sukisu-Theme-Token: $TOKEN" \
  "$BASE/themes/<id>-v1.sukisu" | grep -iE 'HTTP|location'

# 跟随跳转并检查文件大小（--location-trusted 让 curl 也把 token 转发给镜像）
curl -sL --location-trusted -H "X-Sukisu-Theme-Token: $TOKEN" \
  "$BASE/themes/<id>-v1.sukisu" -o /tmp/theme.sukisu
sha256sum /tmp/theme.sukisu

# 带 Range 的请求应返回 200/206，而不是 302
curl -sI -H "X-Sukisu-Theme-Token: $TOKEN" -H "Range: bytes=10-" \
  "$BASE/themes/<id>-v1.sukisu" | head -1
```

服务端 debug 日志会输出：

```text
redirecting theme package mirror=edge-a path=themes/...
```

把 `RUST_LOG` 设为 `info,sukisu_theme_web=debug` 即可看到。
