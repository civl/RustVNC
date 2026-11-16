# RustVNC Agent — A1 控制面

A1 在 A0 骨架上加入 Agent 与客户端之间的**加密控制通道**，并完成第一轮安全收口：

- QUIC + TLS 1.3（ALPN `rustvnc-agent/1`）
- 一次性配对码 + 自签 CA + 客户端证书（mTLS）
- **CSR 配对**：客户端本地生成私钥，只把 CSR 发给 Agent
- **Keychain**：Agent 的 CA/服务器私钥、客户端的私钥优先存登录钥匙串
- **按 IP 限流**：配对尝试与连接建立都有独立窗口
- `Hello / ServerHello` 能力协商
- `Ping / Pong` RTT 测量
- 客户端证书白名单（`clients.txt`）
- 自动化集成测试：配对、Ping、拒绝、限流全链路

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
| `--state-dir` | 证书/白名单位置；默认 `~/Library/Application Support/RustVNC/agent` |
| `--pair-code` | 指定 8 位配对码；不指定则随机生成 |
| `--san` | 追加服务器证书 SAN（Tailscale MagicDNS 名称等），可重复 |
| `--no-keychain` | 私钥改用 0600 文件（测试/CI 用） |

配对码 **5 分钟有效、一次性、全局最多 5 次、单 IP 最多 5 次/10 分钟**。重启 Agent 会生成新的配对码。

## 2. 配对（CSR）

```bash
target/debug/rvncctl pair \
  --addr 127.0.0.1:8443 \
  --code 12345678 \
  --name my-windows-pc \
  --state-dir /tmp/rvnc-a1-client
```

流程：

1. 客户端本地生成密钥对，构造 CSR（CN=设备名、EKU=ClientAuth）。
2. 通过 TLS 发送 `PairRequest { code, client_name, csr_pem }`。
3. Agent 校验配对码与 CSR 签名，签发客户端证书。
4. Agent 返回 `PairResponse { client_cert_pem, ca_cert_pem, server_fingerprint }`。
5. **客户端私钥从未离开客户端**；Agent 只保存证书指纹。

默认写入：

```text
/tmp/rvnc-a1-client/
├─ ca.pem            # Agent CA
└─ client-cert.pem   # 由 Agent CA 签发
# client-key.pem 仅在 --no-keychain 时出现
```

私钥默认存登录 Keychain（service `com.rustvnc.client`，account 由 state-dir 哈希派生）。

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
    PairRequest { code, client_name, csr_pem },
    PairResponse { client_cert_pem, ca_cert_pem, server_fingerprint },
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
| 5 | 尝试次数过多（全局或单 IP） |
| 6 | 协议错误 |
| 7 | 内部错误 |

### 4.3 限流

| 维度 | 限制 |
|---|---|
| 配对码有效期 | 5 分钟 |
| 配对码全局尝试 | 5 次 |
| 单 IP 配对尝试 | 5 次 / 10 分钟 |
| 单 IP 新连接 | 30 次 / 分钟 |
| 未认证连接 | 10 秒内完成握手 |

## 5. 证书与信任模型

```text
Agent 自签 CA（CA 私钥存 Keychain）
  ├─ 服务器证书（SAN: localhost/127.0.0.1/::1；可 --san 追加）
  └─ 客户端证书（配对时由 CSR 签发）
```

- 客户端配对时**接受任意服务器证书**，保存 Agent 返回的 CA；这是为了首次连接可用。
- 因此首次配对必须核对指纹（或 `--expect-fingerprint`），否则无法防主动 MITM。
- 配对完成后，`ping` 用保存的 CA 校验服务器证书，用客户端证书做 mTLS。
- Agent 端用 CA 校验证书链，再按客户端证书 SHA-256 白名单决定是否放行。
- Agent 的 CA/服务器私钥与客户端私钥优先存 Keychain；Keychain 写入失败时回退到 0600 PEM 文件并记录警告。

## 6. 自动化测试

```bash
cargo test
```

覆盖：

- 协议消息 postcard 往返
- 主机名解析
- 限流窗口
- Keychain account 派生
- 全链路集成（`tests/a1_integration.rs`）：
  - 启动服务 → 配对 → Ping
  - 错误配对码
  - 配对码复用
  - 从白名单移除后拒绝
  - 指纹不匹配拒绝
  - 跨 CA 客户端拒绝
  - 单 IP 配对限流

测试统一 `keychain: false`，不会碰开发机的钥匙串。

## 7. 已知限制

- A1 只有 headless `--serve`；菜单栏进程尚未启动 QUIC 服务，下一步接。
- 服务器证书 SAN 默认只有 localhost/127.0.0.1/::1；通过 Tailscale 主机名连接时要用 `--san`。
- 证书生成后修改 `--san` 不会自动生效；删除 `--state-dir` 下的 `server-cert.pem`/`server-key.pem` 后重启即可（CA 与客户端白名单保留）。
- 不要用同一个 `--state-dir` 同时启动两个 `--serve` 进程。
- `clients.txt` 追加式存储；撤销客户端需要手工删除对应行并重启 Agent（后续提供菜单操作）。
- 还没有音频/视频/剪贴板/键鼠，只有控制面。

## 8. 代码位置

```text
src/lib.rs            # 共享库入口
src/protocol.rs       # ControlMsg、帧格式、错误码
src/tls.rs            # CA/服务器/客户端证书、rustls/quinn 配置
src/secrets.rs        # Keychain 存取 + 0600 文件回退
src/server.rs         # QUIC 服务端、配对、白名单、限流、Ping/Pong
src/client.rs         # CSR 配对与 Ping 客户端
src/bin/rvncctl.rs    # 命令行客户端
tests/a1_integration.rs
```

A0 的菜单栏、SMAppService、单实例代码保持不变。
