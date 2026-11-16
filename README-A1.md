# RustVNC Agent — A1 控制面

A1 在 A0 骨架上加入了 Agent 与客户端之间的**加密控制通道**：

- QUIC + TLS 1.3（ALPN `rustvnc-agent/1`）
- 一次性配对码 + 自签 CA + 客户端证书（mTLS）
- `Hello / ServerHello` 能力协商
- `Ping / Pong` RTT 测量
- 客户端证书白名单（`clients.txt`）

音频、剪贴板、视频和键鼠仍属于后续阶段。

## 1. 启动 Agent 控制服务

A1 先用 headless 模式，避免和 A0 菜单栏进程互相干扰：

```bash
cargo build

target/debug/rustvnc-agent --serve \
  --listen 127.0.0.1:8443 \
  --state-dir /tmp/rvnc-a1-agent \
  --pair-code 12345678
```

输出示例：

```text
LISTENING: 127.0.0.1:8443
PAIRING CODE: 12345678
SERVER FINGERPRINT: 8ff70832...
STATE DIR: /tmp/rvnc-a1-agent
```

参数：

| 参数 | 说明 |
|---|---|
| `--listen` | 监听地址；默认 `127.0.0.1:8443` |
| `--state-dir` | CA/服务器证书/客户端白名单位置；默认 `~/Library/Application Support/RustVNC/agent` |
| `--pair-code` | 指定 8 位配对码；不指定则随机生成 |
| `--san` | 追加服务器证书 SAN（Tailscale MagicDNS 名称等），可重复 |

配对码 **5 分钟有效、一次性、最多尝试 5 次**。重启 Agent 会生成新的配对码。

## 2. 配对

```bash
target/debug/rvncctl pair \
  --addr 127.0.0.1:8443 \
  --code 12345678 \
  --name my-windows-pc \
  --state-dir /tmp/rvnc-a1-client
```

成功后会写入：

```text
/tmp/rvnc-a1-client/
├─ ca.pem            # Agent CA，用于校验服务器
├─ client-cert.pem   # 由 Agent CA 签发的客户端证书
└─ client-key.pem    # 客户端私钥（0600）
```

安全建议：把 Agent 打印的 `SERVER FINGERPRINT` 与客户端输出核对一次，或直接传入：

```bash
--expect-fingerprint 8ff70832e2158fea512f1af03cd98d81041278ad8a1d67e557eba9cc3452db39
```

不匹配会拒绝保存。

## 3. Ping / Pong

```bash
target/debug/rvncctl ping \
  --addr 127.0.0.1:8443 \
  --count 3 \
  --state-dir /tmp/rvnc-a1-client
```

输出：

```text
CONNECTED
AGENT: RustVNC Agent 0.1.0
CAPABILITIES: 0x0
pong 0: rtt=0.332 ms
pong 1: rtt=0.230 ms
pong 2: rtt=0.151 ms
```

`ping` 使用 mTLS：Agent 只接受白名单里的客户端证书。

## 4. 协议

### 4.1 传输

- QUIC（`quinn`），UDP 8443
- TLS 1.3 only，ALPN `rustvnc-agent/1`
- 单条双向控制流；后续音视频会另开流
- 帧格式：`u32` 大端长度 + postcard 编码的 `ControlMsg`
- 单帧上限 1 MiB

### 4.2 消息

```rust
enum ControlMsg {
    Hello { proto, client, capabilities },
    ServerHello { proto, agent, capabilities },
    PairRequest { code, client_name },
    PairResponse { client_cert_pem, client_key_pem, ca_cert_pem, server_fingerprint },
    Ping { nonce },
    Pong { nonce },
    Goodbye,
    Error { code, message },
}
```

错误码：

| 码 | 含义 |
|---|---|
| 1 | 未授权（没有客户端证书 / 不在白名单） |
| 2 | 配对码错误 |
| 3 | 配对码过期 |
| 4 | 配对码已使用 |
| 5 | 尝试次数过多 |
| 6 | 协议错误 |
| 7 | 内部错误 |

## 5. 证书与信任模型（A1 简化）

```text
Agent 自签 CA（ca-cert.pem + ca-key.pem）
  ├─ 服务器证书（server-cert.pem，SAN: localhost/127.0.0.1/::1）
  └─ 客户端证书（配对时签发）
```

- 客户端配对时**接受任意服务器证书**，然后保存 Agent 返回的 CA；这是为了首次连接可用。
- 因此首次配对必须核对指纹（或 `--expect-fingerprint`），否则无法防主动 MITM。
- 配对完成后，`ping` 用保存的 CA 校验服务器证书，用客户端证书做 mTLS。
- Agent 端用 CA 校验证书链，再按客户端证书 SHA-256 白名单决定是否放行。
- 私钥目前是 0600 的 PEM 文件；A3 会迁移到 Keychain/DPAPI，并改用 CSR。

## 6. 已知限制

- A1 只有 headless `--serve`；菜单栏进程尚未启动 QUIC 服务，下一步接。
- 客户端私钥由 Agent 生成后通过 TLS 下发；A3 改为客户端生成密钥 + CSR。
- 服务器证书 SAN 默认只有 localhost/127.0.0.1/::1；通过 Tailscale 主机名连接时要用 `--san` 追加。
- 证书生成后修改 `--san` 不会自动生效；删除 `--state-dir` 下的 `server-cert.pem`/`server-key.pem` 后重启即可（CA 与客户端白名单保留）。
- 不要用同一个 `--state-dir` 同时启动两个 `--serve` 进程。
- 还没有音频/视频/剪贴板/键鼠，只有控制面。
- `clients.txt` 追加式存储；撤销客户端需要手工删除对应行并重启 Agent（A3 提供菜单操作）。

## 7. 代码位置

```text
src/lib.rs            # 共享库入口
src/protocol.rs       # ControlMsg、帧格式、错误码
src/tls.rs            # CA/服务器/客户端证书、rustls/quinn 配置
src/server.rs         # QUIC 服务端、配对、白名单、Ping/Pong
src/client.rs         # 配对与 Ping 客户端
src/bin/rvncctl.rs    # 命令行客户端
```

A0 的菜单栏、SMAppService、单实例代码保持不变。
