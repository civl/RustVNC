# RustVNC 威胁模型（M3）

> 范围：Windows 客户端 `rustvnc-client`、macOS 被控端 `rustvnc-agent`、两者之间的
> QUIC 控制通道，以及它们使用的 RFB/SSH 传输。
> 方法：STRIDE + 资产/信任边界清单。每条缓解措施都尽量指向可验证的实现或测试。

## 1. 资产

| 资产 | 位置 | 泄露后果 |
|---|---|---|
| macOS 账号密码 | Windows 凭据管理器 / 进程内存 | 直接登录被控 Mac |
| Agent 私钥（CA + 服务端） | Mac `~/Library/Application Support/RustVNC/agent/*.pem` | 可冒充 Agent |
| 已配对客户端证书 | 同上 `clients.txt`（指纹） | 可冒充已授权客户端 |
| 画面与按键 | RFB 流 | 隐私泄露 |
| 剪贴板内容 | Agent 通道 | 密码/令牌泄露 |
| 审计日志 | `%APPDATA%\RustVNC\client\audit.jsonl` | 行为画像（不含密钥） |

## 2. 信任边界

```text
[Windows 桌面用户]
      │ ①本地进程/凭据管理器
[rustvnc-client] ──②RFB(5900，可明文)── [screensharingd]
      │
      └──③QUIC+TLS1.3+mTLS(8443，UDP)── [rustvnc-agent] ──④TCC 权限── macOS 系统
```

- ②是**用户可选的降级路径**：明文时任何能抓包的人都能看到画面和按键。M3 用"每
  profile 显式确认 + 审计"把它变成有意识的选择（`show_plaintext_confirm`）。
- ③的信任根是**配对时固定的服务端指纹**（`state.rs` 的 `server-fingerprint.txt`），
  指纹变化直接拒绝，不静默重连。

## 3. STRIDE

| 威胁 | 场景 | 缓解 | 状态 |
|---|---|---|---|
| **S**poofing | 局域网内伪造 Agent 骗取配对码 | mTLS + 配对码一次性 + 服务端指纹固定 | ✅ 已实现 |
| **S**poofing | 攻击者拿到私钥冒充 Agent | 私钥 0600，仅存用户目录；轮换用 `--rotate-ca` | ✅ |
| **T**ampering | 中间人改画面数据 | TLS 1.3 AEAD；明文模式下无保护（用户已确认） | ✅ / 设计取舍 |
| **T**ampering | 恶意服务端发畸形矩形/长度 | 解析器长度上限 + fuzz 目标 + 属性测试 | 🟡 有上限与 fuzz，24h 长跑待跑 |
| **R**epudiation | 事后无法确认谁连过 | `audit.jsonl` 记录连接/断开/主机密钥/明文接受/更新 | ✅ |
| **I**nformation disclosure | 崩溃转储/日志泄露密码 | panic 记录只写位置+脱敏消息；无 minidump；日志过滤 | ✅ |
| **I**nformation disclosure | 抓包看到明文画面 | 默认推荐 SSH 隧道；明文需显式确认并审计 | ✅ |
| **D**enial of service | 恶意客户端刷配对码 | 5 次全局 + 每 IP 5 次/10 分钟限流 | ✅ |
| **D**enial of service | 超大帧内存耗尽 | `MAX_PIXELS` / `MAX_COMPRESSED` / 伪编码 payload 上限 | ✅ |
| **E**levation of privilege | DLL 劫持（放一个同名 DLL 到工作目录） | `SetDefaultDllDirectories` 排除当前目录 | ✅ |
| **E**levation of privilege | 子进程（ssh）在客户端崩溃后残留 | Job Object `KILL_ON_JOB_CLOSE` | ✅ |
| **E**levation of privilege | 内存破坏利用 | DEP/ASLR/HighEntropyVA/CFG/CET 链接标志 | 🟡 见 §4 |
| **E**levation of privilege | 供应链：依赖带漏洞 | `cargo-deny`（CI）+ `cargo-audit`（CI）+ SBOM | ✅ |

## 4. 已知残余风险

1. **CFG 未插桩**：PE 头已置 `GUARD_CF` 且存在 Load Config，但 `GuardFlags` 无
   `CF_INSTRUMENTED` —— Rust 稳定版不提供编译器级 CFG 插桩。实际收益主要来自
   DEP/ASLR/CET。要去掉这个残余项需要 nightly 的 `-Z cf-protection` 或 MSVC 侧
   静态库改造，暂不引入 nightly 到生产构建。
2. **同用户攻击者**：能读用户目录的进程可以直接读 CA 私钥与审计日志。这是桌面
   应用的固有边界，不通过提权缓解。
3. **明文模式**：确认过的 profile 之后不再提示（只审计），这是有意识的取舍。
4. **吊销不打断已建立连接**：`--revoke` 后新握手被拒，但已开的会话延续到断开。
5. **剪贴板**：内容经 mTLS 传输，但落在对端系统剪贴板里，无法防止对端其他程序读取。

## 5. 评审记录

- 首次 STRIDE 评审：2026-11-20（本文件）。
- 外部渗透测试：尚未进行，见 [PENTEST-PLAN.md](PENTEST-PLAN.md)。
