# Android 投屏发送端 · 验证记录（2026-10-30）

## 1. 交付物

| 内容 | 位置 |
| --- | --- |
| 发送端工程 | [`android/`](../android/)，含 Gradle wrapper、单测、README |
| Debug APK | `android/app/build/outputs/apk/debug/app-debug.apk`（853,467 B，sha256 `08555d0d…dc33`） |
| Release APK | `android/app/build/outputs/apk/release/app-release.apk`（652,765 B，sha256 `9c047b66…fcfd`） |
| 联调脚本 | [`tools/cdp-eval.mjs`](../tools/cdp-eval.mjs)、[`tools/cdp-shot.mjs`](../tools/cdp-shot.mjs)（走 CDP 驱动大屏） |

协议未做大屏侧任何改动：手机端就是浏览器扩展的另一个实现，接收端仍是
`crates/horizon-cast/src/browser.rs` 的 9407 服务。

## 2. 环境

| 组件 | 版本/位置 |
| --- | --- |
| JDK | 17.0.21（`C:\Program Files\Microsoft\jdk-17.0.21.21-hotspot`） |
| Android SDK | platform 35 / build-tools 35.0.0 / platform-tools 37.0.1（`D:\AndroidSdk`） |
| Gradle | 8.9（`D:\gradle-dist`，仓库同时生成 wrapper） |
| AGP / Kotlin | 8.7.3 / 2.0.21，minSdk 26，targetSdk 35 |

过程中踩到的两个环境坑（已写进 `android/README.md`）：

1. `services.gradle.org` 在本机被阻断，Gradle 发行包改从腾讯镜像下载（`unzip -t` 校验通过）。
2. 用户名含中文时，Gradle 测试 worker 的 `@argfile` 编码解析失败，报
   `ClassNotFoundException: GradleWorkerMain`；把 `GRADLE_USER_HOME` 指到 `D:\gradle-home`
   后正常。这只影响测试 worker，不影响 APK 构建。

## 3. 协议对照

| 浏览器扩展 | Android 本实现 |
| --- | --- |
| `tabCapture` / `getDisplayMedia` | `MediaProjection` + `VirtualDisplay`（`AUTO_MIRROR`） |
| `VideoEncoder`（WebCodecs H.264） | `MediaCodec` `video/avc`，surface 输入，CBR |
| 30 fps 上限、IDR 5 s、码率 `clamp(w·h·fps·0.12,1.5–12 Mbps)` | 相同公式（`VideoEncoder.bitrate`） |
| 关键帧前补 SPS/PPS | `csd-0/csd-1` 缓存 + 每个 IDR 前置 |
| 24 B 小端头，`flags` bit1=墙钟 pts | `Wire.kt`；pts 由单调时钟换算到墙钟 |
| `hello` → `welcome`；收 `{"type":"keyframe"}` 立即出 IDR | `WsClient` + `MirrorService.socketListener` |
| `/24` 扫 `/info`，404 视为旧版 | `Discovery` |
| 音频 PCM kind=2 | **未实现** |

## 4. 单元测试（无设备）

```
./gradlew testDebugUnitTest
BUILD SUCCESSFUL — 17 tests, 0 failures
```

| 用例 | 断言重点 |
| --- | --- |
| `WireTest.headerLayout` | 24 字节头逐字段、1920×1080、长度、载荷追加位置 |
| `WireTest.audioPacket` | 采样率/声道放在 12/14 字节 |
| `AnnexBTest` ×5 | Annex B 直通、3 字节 start code、长度前缀转换、截断保护、SPS 补 start code |
| `WsClientTest.handshake…` | 请求行带 `?token`，客户端帧确实加掩码，文本/二进制往返，收到关键帧请求，close 帧收尾 |
| `WsClientTest.wrongToken` / `missingStream` | 403 → 「投屏码不正确」，404 → 「接收端没有开启投屏」 |
| `DiscoveryTest` ×6 | `/info` 正常/404/异类服务/500/端口关闭/排序与候选集 |

代码审查阶段顺带修掉的三个缺陷：

- `Discovery.sweep` 的「取消标志」被传成了「正在扫描」，导致 worker 立刻退出、永远扫不到；
  改为独立的 `cancelScan`。
- 通知 `Action.Builder` 第一个参数是 `int` 图标，传 `null` 会在运行期崩；改用系统图标。
- `MirrorState.status` 属性与 `fun setStatus()` 生成同一个 JVM 签名，编译期冲突；改为 `live()`。

另外，`WsClient` 从 `android.util.Base64` 换成 `java.util.Base64`（minSdk 26 起可用），
这样 `Wire`/`WsClient`/`AnnexB`/`Discovery` 都是纯 JVM 代码，协议层可以脱离手机验证。

## 5. 真实接收端联调（不需要手机的部分）

做法：启动器（`target/release/horizon-launcher.exe`）经 CDP 打开「投屏 → 接收 → 浏览器投屏」，
投屏码 `504282`，接收端在 `127.0.0.1:9407` 与 `192.168.0.27:9407` 同时应答
`{"name":"戎耀会","paired":true,"path":"/stream","port":9407,"service":"horizon-browser"}`。
随后用 `ReceiverLiveTest` 以**与 APK 相同的 `Wire`/`WsClient` 代码**投一段真实 H.264 裸流：

```
./gradlew testDebugUnitTest -PhorizonToken=504282 -PhorizonH264=/tmp/horizon-test.h264
live stream sent: 180 frames (6 keyframes) to 127.0.0.1:9407
BUILD SUCCESSFUL
```

大屏侧实时读数（CDP 抓取，非源码推断）：

| 观察点 | 结果 |
| --- | --- |
| 画面事实行 | `1280×720 · 180 帧 · 6 关键帧` |
| 计数条 | `180 帧 / 2.6 MB 视频 / 0 B 音频`（源文件 2,675,181 B） |
| 预览画布像素 | 425×239，采样 14 种颜色分桶，均值 (124,133,123)——不是空帧 |
| 错误投屏码 | 用 `000000` 连接，客户端抛出「投屏码不正确」（服务端 403） |
| 会话结束 | 流发完主动关闭后，`/info` 仍在线，面板回到等待连接状态 |

预览窗截图存于 `.probe/android-live-preview.png`（本机证据，未入库），
画面是 ffmpeg `testsrc2` 彩条，叠加着接收端自己的投屏码提示。

## 6. 构建产物核对

| 产物 | 大小 | 说明 |
| --- | --- | --- |
| `app-debug.apk` | 853,467 B | 含调试信息 |
| `app-release.apk` | 652,765 B | `isMinifyEnabled=false`，复用 debug 签名，可直接 `adb install` |

两者均通过 `zipinfo`/安装包解析前的 `assemble` 校验；`AndroidManifest` 已声明
`FOREGROUND_SERVICE_MEDIA_PROJECTION`、`POST_NOTIFICATIONS`、`usesCleartextTraffic`，
服务声明 `foregroundServiceType="mediaProjection"`。

## 7. 这一轮没有覆盖到的（诚实边界）

- **没有真机**：`adb devices` 全程为空，因此 `MediaProjection` 授权、`MediaCodec` 的实际输出
  （是否 Annex B、csd 何时出现、旋转重建是否成功）、前台服务在 ColorOS/MIUI/HyperOS 上的存活，
  都还没有在手机上跑过。第 5 节的联调证明的是**协议与传输**，用的是 JVM 侧同源代码，
  不是手机编码器产物。
- **音频**：本版只发画面（kind=1），接收端会显示 `0 B 音频`。
- **断线重连**：接收端关闭后 App 显示断开原因并停止，需要重新点「开始投屏」。
- **延迟**：手机→大屏的端到端延迟未测，因为缺少真机；协议里已经带上墙钟 pts，
  真机接通后可直接用接收端的延迟读数补测。
- **模拟器也走不通**：为补上真机缺口，装了 `emulator 37.2.13` 与
  `system-images;android-35;default;x86_64`（约 2 GB），建了 `medium_phone` AVD。四组组合全部
  在初始化加速器后立刻段错误（退出码 139）：WHPX + `-no-window` + swiftshader、WHPX + 窗口 +
  `-gpu host`、WHPX + 窗口 + swiftshader、`-accel off`（TCG）。日志停在
  `WHPX on Windows 10.0.26200 detected` / `Windows Hypervisor Platform accelerator is operational`
  之后，adb 始终 `offline`，一次都没进到系统。这是本机 Hyper-V/WHPX 与这版模拟器的组合问题，
  不是 App 的问题；换模拟器版本或改用真机都可绕开。

## 8. 复现步骤

```bash
# 1. 构建
cd android
GRADLE_USER_HOME=/d/gradle-home ./gradlew assembleDebug assembleRelease

# 2. 协议单测
GRADLE_USER_HOME=/d/gradle-home ./gradlew testDebugUnitTest

# 3. 对真实接收端（大屏已开「浏览器投屏」）
GRADLE_USER_HOME=/d/gradle-home ./gradlew testDebugUnitTest \
  -PhorizonToken=<6位投屏码> -PhorizonH264=/tmp/horizon-test.h264

# 4. 真机
adb install -r app/build/outputs/apk/release/app-release.apk
adb shell am start -n com.horizon.castsender/.MainActivity
adb logcat -s HorizonCast   # 若后续加日志 tag；当前用 App 内滚动日志与接收端计数核对
```
