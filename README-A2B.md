# RustVNC Agent — A2b Core Audio Tap + Opus

A2b 在控制面/剪贴板之上加入系统音频采集与编码：

- **Core Audio Process Tap**（macOS 14.4+）采集系统输出
- 48 kHz / 立体声 / 20 ms 帧
- **Opus** 64 kbps、VBR、带内 FEC
- 通过 **QUIC datagram** 发送，不阻塞控制流
- 只有客户端请求音频时才创建 tap；Ping 不会触发录音

## 1. 数据链路

```text
macOS 系统输出
  └─ Core Audio Process Tap（排除自身进程）
       └─ 私有 aggregate device → IOProc → SPSC ring
            └─ 48 kHz stereo f32
                 └─ Opus 20 ms 帧（960 samples）
                      └─ AudioPacket（7 字节头 + payload）
                           └─ QUIC datagram → 客户端
```

音频包格式：

```text
u8  version = 1
u16 seq       (LE)
u32 pts_ms    (LE)
u8  payload[] (Opus)
```

服务端在 `ServerHello.capabilities` 中设置 `CAP_AUDIO = 0x1`，表示本次会话音频已成功启动。

## 2. 权限

系统音频采集需要 **System Audio Recording** 权限（TCC `kTCCServiceAudioCapture`），Info.plist 已包含：

```xml
<key>NSAudioCaptureUsageDescription</key>
<string>RustVNC Agent 需要捕获系统音频，以便将 Mac 的声音传输到你的 Windows 客户端。</string>
```

- 首次创建 tap 时系统可能弹权限提示。
- 如果拒绝，`AudioCapture::start` 返回错误，`CAP_AUDIO` 为 0；`rvncctl audio` 会报：

  ```text
  agent did not start audio capture (permission or tap error)
  ```

- 可在“系统设置 → 隐私与安全性 → 屏幕与系统音频录制”中授权。
- Core Audio Process Tap 要求 **macOS 14.4+**；更早版本 Ping/剪贴板仍可用，但没有音频。

## 3. 使用

Agent（菜单栏或 headless）默认允许音频。headless 可显式关闭：

```bash
target/debug/rustvnc-agent --serve ... --no-audio
```

GUI 可用环境变量关闭：

```bash
RUSTVNC_NO_AUDIO=1
```

客户端：

```bash
# 先配对
target/debug/rvncctl pair --addr 127.0.0.1:8443 --code 12345678 ...

# 接收并解码 Opus；打印包数/RMS
target/debug/rvncctl audio --addr 127.0.0.1:8443 --seconds 5 --state-dir ...
```

播放点声音再跑，例如：

```bash
(sleep 1; say "Rust VNC audio test") &
target/debug/rvncctl audio --seconds 5 --state-dir ...
```

输出示例：

```text
audio: packets=50 seq=49 bytes=6873
audio: packets=100 seq=99 bytes=14784
audio summary: packets=166 samples=159360 bytes=17632 rms=0.09073
```

`rvncctl` 目前只解码并统计 RMS，不做本地播放；Windows 客户端的 WASAPI 播放与抖动缓冲是下一步。

## 4. 已验证

- 打包后的 `.app` 执行 `--serve`，配对成功。
- `ping` 的 `CAPABILITIES` 为 `0x0`，确认不会触发 tap。
- `rvncctl audio` 请求 `CAP_AUDIO` 后，Agent 启动 tap，`ServerHello` 返回 `0x1`。
- 播放系统语音期间收到 166 / 211 个 Opus 包（5–6 秒），解码 RMS ≈ 0.09–0.12，无序列号告警。
- Opus 编解码单元测试通过。
- 控制面/剪贴板集成测试仍全部通过。

## 5. 当前限制

- 只采集 **默认输出设备**；切换输出设备后需要重开会话（后续监听设备变化并重建 tap）。
- 没有客户端抖动缓冲/丢包隐藏/音画同步；Windows 客户端实现时补齐。
- `rvncctl audio` 不播放声音，只解码统计。
- 固定 48 kHz / 立体声 / 20 ms / 64 kbps；后续可做码率自适应。
- 首次 tap 创建可能耗时 3 秒左右（权限检查），后续会话约几十毫秒。
- 无麦克风上行。

## 6. 代码位置

```text
src/audio.rs       # Core Audio Tap、SPSC ring、Opus 编码、音频线程
src/protocol.rs    # CAP_AUDIO、AudioPacket 编解码
src/server.rs      # 按 CAP_AUDIO 启动 tap、QUIC datagram 转发
src/client.rs      # rvncctl audio：收包、Opus 解码、RMS
src/bin/rvncctl.rs # audio 子命令
```
