# HORIZON 投屏发送端（Android）

把 Android 手机屏幕投到 HORIZON 大屏。协议与 `extension/` 里的 Chrome/Edge 扩展完全一致，
接收端是启动器里「投屏 → 浏览器投屏」那个 9407 服务，**大屏侧不需要任何改动**。

```
MediaProjection ──▶ VirtualDisplay ──▶ MediaCodec(H.264, surface 输入) ──▶ Annex B
                                                                              │
                      24 字节小端头 + 帧数据 ──▶ WebSocket ──▶ ws://大屏:9407/stream?token=投屏码
```

## 已实现

- **发现**：扫本机所在 /24 的 `http://<ip>:9407/info`（700 ms 超时、并发扫描），
  `service == "horizon-browser"` 即接收端；404 视为「旧版接收端」仍然列出。`/info` 里的
  `hub` 字段是片库端口（缺省 7878），发现结果随接收端一起保存。
- **投屏码**：手输 6 位，作为 `/stream?token=`；错误码在 UI 上直接显示「投屏码不正确」。
- **媒体片库与队列**：媒体页连上主机后，可多选手机上的视频/音乐/图片一次性上传进大屏片库，
  「播放所选」把整批设为队列并开播、「追加到队列」不打断当前播放；片库条目可单条投/追加/删除
  （删除连带移出队列）；队列可 ↑↓ 排序、⇥ 预排、✕ 移除、点行跳转，播放/暂停/停止走同一个
  `transport`。所有动作与大屏 IPC、浏览器扩展是**同一个** `MediaStage` dispatcher，队列每秒轮询一次。
- **DLNA 直投**：保留单条「手机起流 + DLNA SetAVTransportURI」作为第二来源，界面标注它依赖手机保持可达。
- **编码**：AVC，`COLOR_FormatSurface`，CBR，帧率 30，关键帧间隔 5 s，
  码率 `clamp(w·h·fps·0.12, 1.5 Mbps, 12 Mbps)`；分辨率按 1920 长边等比缩放并对齐偶数。
- **封装**：24 字节小端头（kind/flags/pts_us/w/h/len），`flags` bit0=关键帧、bit1=pts 为发送方墙钟；
  每个 IDR 前补 SPS/PPS（与 `extension/offscreen.js` 同策略）。
- **时间戳**：合成器给帧打的是单调时钟，按会话起点换算成墙钟微秒，接收端做延迟/新鲜度判断仍然有效。
- **会话**：`hello` 握手、接收端 `{"type":"keyframe"}` → 立即请求 IDR、前台服务常驻通知、
  通知里可直接停止、旋转后自动按新尺寸重建编码器与虚拟屏；断线按 1/2/4/8/8 s 退避自动重连，
  保留 MediaProjection 令牌只重建编解码器，最多 5 次。

## 尚未实现（有意留到下一步）

- **音频**：浏览器扩展还有 PCM 音轨（kind=2），Android 侧要 `AudioPlaybackCapture` +
  `AudioRecord`，只对允许被捕获的应用生效；本版只投画面。
- **多显示器/外接屏**：按默认屏镜像，暂不选屏。
- **片库上传断点续传**：一次定长请求，断网需重选。

## 目录

| 文件 | 职责 |
| --- | --- |
| `Wire.kt` | 24 字节帧头 |
| `AnnexB.kt` | start code 规范化、长度前缀转 Annex B |
| `WsClient.kt` | 最小 RFC 6455 客户端（握手、掩码、ping/pong、文本/二进制） |
| `Discovery.kt` | /24 扫描 `/info`、接收端排序、解析 `hub` 端口 |
| `LanDiscovery.kt` | 后台扫描与片库服务探测（7878） |
| `HubMediaClient.kt` | 主机片库/队列/传输的 HTTP 客户端（含 multipart 上传） |
| `VideoEncoder.kt` | MediaCodec + VirtualDisplay + 取出码流 |
| `MirrorService.kt` | 前台服务：串起上面所有环节，16 帧发送队列、超时丢帧、重连 |
| `MirrorState.kt` | 进程内状态与滚动日志 |
| `pages/MediaPage.kt` | 媒体页外壳：片库区块 + DLNA 直投 |
| `pages/MediaHub.kt` | 片库主机连接、多选投放、片库与队列管理 |
| `MainActivity.kt` | 纯平台控件搭建的界面（无 AndroidX） |

## 构建

要求：JDK 17、Android SDK Platform 35、Build-Tools 35、Gradle 8.9（仓库已带 wrapper）。

```bash
cd android
# 指向本机 SDK，或者设置 ANDROID_HOME
printf 'sdk.dir=%s\n' "$PWD/../AndroidSdk" > local.properties   # 路径按实际情况写

./gradlew assembleDebug     # app/build/outputs/apk/debug/app-debug.apk
./gradlew assembleRelease   # 侧载用，复用 debug 签名，可以直接安装
```

- 国内网络如果 `services.gradle.org` 不通，把 `gradle/wrapper/gradle-wrapper.properties` 的
  `distributionUrl` 换成镜像（例如 `https://mirrors.cloud.tencent.com/gradle/gradle-8.9-bin.zip`）。
  如果 wrapper 仍反复重下发行包，直接用手上的 Gradle 8.9 构建，可以完全跳过 wrapper：
  `gradle -p android :app:assembleRelease`。
- **Windows 中文用户名**：Gradle 测试 worker 用 `@argfile` 传 classpath，用户名含中文时
  worker 会因编码问题起不来（`ClassNotFoundException: GradleWorkerMain`）。
  构建/测试时把 `GRADLE_USER_HOME` 指到纯 ASCII 路径即可，例如 `D:\gradle-home`。

## 使用

1. 大屏：启动器 → 投屏 → 接收 → 打开「浏览器投屏」，记下 6 位**投屏码**。
2. 手机与大屏连同一个 Wi-Fi；打开 App，列表里会出现接收端（也可手填地址）。
3. 输入投屏码 → 开始投屏 → 允许录屏。
4. 大屏「画面 1280×720 · N 帧 · M 关键帧」持续增长即接通。

```bash
adb install -r app/build/outputs/apk/release/app-release.apk
```

## 测试

```bash
./gradlew testDebugUnitTest        # 59 项，3 项 live 默认跳过，无需手机与接收端
```

单测覆盖：帧头字节序、Annex B 两种形态、WebSocket 握手/掩码/文本/关键帧请求/403/404、
`/info` 三种应答与 `hub` 字段、接收端排序、片库/队列 JSON 解析、队列动作报文、
multipart 上传逐字节与错误文案。两项 live 用例只在给了参数时才跑：

```bash
# 真实接收端：直接发一段 Annex B 裸流
./gradlew testDebugUnitTest -PhorizonToken=504282 -PhorizonH264=/tmp/clip.h264

# 真实主机片库：上传两条素材 → setQueue → 暂停/继续 → 排序/跳转/移除 → 清理
./gradlew testDebugUnitTest -PhorizonHub=192.168.1.5:7878 \
  --tests 'com.horizon.castsender.HubMediaLiveTest'
```

`HubMediaLiveTest` 会**接管大屏当前播放的队列**并在结束时清空，只在自己的上传上删条，
适合可清空队列的联调环境。

`clip.h264` 可以是任意 Annex B 裸流，例如：

```bash
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -t 6 \
  -c:v libx264 -profile:v baseline -g 30 -x264-params aud=1:repeat-headers=1 -f h264 clip.h264
```

## 平台注意

- Android 14+：必须先 `startForeground(..., mediaProjection)` 再 `getMediaProjection()`，
  且要在 `createVirtualDisplay()` 前注册 `MediaProjection.Callback`，本实现按此顺序。
- Android 15：每次投屏都要用户确认，无法「记住授权」。
- 明文 HTTP/WS 需要 `usesCleartextTraffic="true"`（仅在局域网内使用）。
- 部分 OEM 在省电策略下会杀掉前台服务，长时间投屏建议加白名单。
