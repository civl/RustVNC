# M3 · 安全加固 交付报告

> 设计出口标准（`design.md` §7.2 M3）：
> 无高危/严重漏洞；TLS 模式下无明文画面/按键/凭据；磁盘与日志无明文密码。
> 本文件逐条给出实现位置与可复现的验证方式，并把"还没做的"单列。

## 1. 工作项与状态

| # | M3 工作项 | 状态 | 实现/证据 |
|---|---|---|---|
| 1 | QUIC/TLS 1.3、mTLS、配对码、证书固定 | ✅ 已完成（早于 M3） | `src/tls.rs`、`crates/agent-client/src/tls.rs`；ALPN `rustvnc-agent/1` |
| 2 | 证书轮换 | ✅ | `rustvnc-agent --rotate-ca --yes`：备份旧材料到 `pre-rotation-<ts>/`，重建 CA 与服务端证书 |
| 3 | 吊销 | ✅ | `rustvnc-agent --revoke <指纹>`（`server::revoke_client`），3 个单测覆盖"只删指定条目/未知指纹无副作用/无文件不报错"；已建连接延续到断开 |
| 4 | STRIDE 威胁建模 | ✅ | [THREAT-MODEL.md](THREAT-MODEL.md)（12 条威胁 × 缓解 × 状态） |
| 5 | 外部渗透测试 ≥1 轮 | ⬜ 待外部执行 | 范围与验收口径已备：[PENTEST-PLAN.md](PENTEST-PLAN.md) |
| 6 | 解析器 fuzz | 🟡 目标与 CI 就绪 | `fuzz/`（Apple 伪编码、Agent postcard）；CI `fuzz` job 各 45 s；24 h 长跑见 §3 |
| 7 | `cargo-audit` / `cargo-deny` 清零高危 | ✅ 接入 CI | CI `deny` + `security`；`cargo deny check` 本地 exit 0（advisories/bans/licenses/sources 全 ok） |
| 8 | CFG/CET 链接加固 | 🟡 标志已置位，未插桩 | `.cargo/config.toml`；`/GUARD:CF /CETCOMPAT /HIGHENTROPYVA`，见 §2 |
| 9 | DLL 搜索路径排除当前目录 | ✅ | `hardening.rs`：`SetDefaultDllDirectories(SYSTEM32 | USER_DIRS)` |
| 10 | 子进程生命周期（Job Object） | ✅ | `hardening.rs`：`KILL_ON_JOB_CLOSE` + 自进程加入 job |
| 11 | 崩溃转储脱敏 | ✅ | `hardening.rs`：panic 只写位置 + 脱敏消息到 `last-panic.txt`，不写 minidump/环境 |
| 12 | 明文模式确认流程 | ✅ | `plaintext_confirm_window`：按 profile 确认一次，可一键改 SSH；决策入审计 |
| 13 | 审计日志 | ✅ | `audit.rs`：`audit.jsonl` 追加写、4 MiB 轮转、密钥字段恒定脱敏 |
| 14 | SBOM 生成与发布 | ✅ | `scripts/sbom.py` → CycloneDX 1.5（632 组件）；CI 产物 + Release 资产 |

## 2. 加固标志的实测结果

对 `target/release/rustvnc-client.exe` 解析 PE 头（`python` 脚本见提交历史）：

```text
PE32+ | sections: 5
  HIGH_ENTROPY_VA        YES
  DYNAMIC_BASE (ASLR)    YES
  NX_COMPAT (DEP)        YES
  GUARD_CF               YES
  CET_COMPAT             YES
  Load Config directory  rva=0x10f4c20 size=320
  GuardFlags             0x1   CF_INSTRUMENTED=no
```

`GuardFlags` 无 `CF_INSTRUMENTED`：Rust 稳定版不提供编译器级 CFG 插桩，链接标志只能
声明兼容性。要真正插桩需 nightly `-Z cf-protection` 或使用 MSVC 编译的静态库，
因此**把 CFG 记为部分完成**，不夸大为"已开 CFG"。

## 3. fuzz 的现状与 24 小时跑法

已就位：

```bash
rustup toolchain install nightly
cargo +nightly install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz run apple_metadata -- -max_total_time=45
cargo +nightly fuzz run control_msg    -- -max_total_time=45
```

- CI 每次 PR 跑 45 s × 2 目标（快速回归）；夜间 workflow 每目标 55 min。
- **Windows 本机跑法**（`scripts/fuzz-local.ps1`）：cargo-fuzz 在 MSVC 上需要
  `clang_rt.asan_dynamic-x86_64.dll` 在 `PATH`，否则 fuzz 目标以
  `STATUS_DLL_NOT_FOUND (0xc0000135)` 退出。脚本会自动定位该目录。
- **24 小时战役**：2026-11-20 21:34 启动，两目标各 24 h（并行，各约 1 核）。
- 24 小时验收跑（本地或专用机）：

```bash
cargo +nightly fuzz run apple_metadata -- -max_total_time=86400 -rss_limit_mb=4096
```

至今未发现崩溃；24 h 连续跑属于**未执行**项，不能算通过。

## 4. 审计日志字段

```json
{"ts":1795000000000,"event":"connect","host":"100.111.251.126","port":"5900","auth":"Ard","transport":"Ssh","bpp":"16"}
```

| event | 含义 |
|---|---|
| `connect` / `disconnect` | 会话起止（含 host/port/auth/transport/bpp） |
| `host-key-trusted` | TOFU 接受新主机密钥 |
| `plaintext-accepted` / `plaintext-declined` / `plaintext-blocked` | 明文传输决策 |
| `share` | 被控端开/关 |
| `update-install` / `update-rollback` | 更新动作 |

按字段名脱敏：`password/passwd/secret/token/private` 的值恒写 `<redacted>`；
正文里形似密钥的长串同样替换。

## 5. 依赖审计结果（`cargo audit`，2026-11-20）

`cargo audit` 报 1 个漏洞、6 个 unmaintained、10 个 unsound，全部为传递依赖：

| 项 | 结论 |
|---|---|
| **RUSTSEC-2026-0293** `ringbuf` 0.4.8（双重释放/UAF，`Consumer::skip`/`clear`） | **评估为不可达**：`flexaudio-core 0.8.1` 只调用 `pop_slice`/`try_pop`/`push_overwrite`，源码中无 `skip`/`clear` 调用点；且需元素 `Drop` panic，我们的元素是纯 `f32/u16` 结构。记录在 `.cargo/audit.toml`，待 flexaudio 升级到 ringbuf 0.5 后删除该豁免 |
| `atomic-polyfill`、`instant`、`paste`、`proc-macro-error`、`rustls-pemfile`、`ttf-parser` | unmaintained，均非直接依赖、无已知安全问题；随上游更新自然消失 |
| `glib` ×1、`heapless` ×9 | unsound 警告；`glib` 走 GTK 编辑器（非发布路径），`heapless` 仅经网络栈的固定容量容器使用，未触及告警中的 `u16::MAX` 边界 |

`cargo audit` 现在 **exit 0**（豁免有据可查），CI `security` 作业不再因它变红。

## 6. 依赖来源与凭据扫描

| 措施 | 实现 |
|---|---|
| 只允许 crates.io | `deny.toml` `[sources] unknown-registry = "deny"` |
| 禁止未知 git 源 | `[sources] unknown-git = "deny"` |
| 禁止 `*` 版本要求 | `[bans] wildcards = "deny"` |
| 提交里不能有密钥 | CI `secrets` 作业（gitleaks，全历史扫描） |

## 7. 尚未完成

1. **外部渗透测试**（需外部团队，计划已备）。
2. **24 小时 fuzz 连续跑**（目标与 CI 已就位，长跑未执行）。
3. **CFG 插桩**（见 §2，需要 nightly 或 MSVC 改造）。
4. **EV 证书**（属 M2d，但直接影响 M3 的"供应链/发布"完整性）。
