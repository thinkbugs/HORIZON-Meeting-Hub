# Android 投屏发送端 · 验证记录（2026-10-30）

本文分三轮：第一轮在没有手机的条件下验证协议与传输；第二轮在真机上把
发现 → 连接 → 编码 → 投屏 → 断线重连 → 旋转 → 长时间空闲整条链路跑通，
并对「有点慢、有些卡」做了量化归因；第三轮把媒体页接上大屏片库与播放队列
（多选上传、排序、传输控制），对齐浏览器扩展（见 §11）。

## 1. 交付物

| 内容 | 位置 / 数值 |
| --- | --- |
| 发送端工程 | [`android/`](../android/)，含 Gradle wrapper、单测、README |
| Release APK（0.1.7 / versionCode 8） | `android/app/build/outputs/apk/release/app-release.apk`，1,005,389 B，sha256 `01903fc3…f84ec6f8`（含第三轮片库/队列，见 §11） |
| 已安装真机版本 | `com.horizon.castsender` versionCode 6 / versionName 0.1.5（0.1.6、0.1.7 均未安装，见 §9） |
| 联调脚本 | [`tools/cdp-eval.mjs`](../tools/cdp-eval.mjs)、[`tools/cdp-shot.mjs`](../tools/cdp-shot.mjs)（走 CDP 驱动大屏） |
| 图标生成脚本 | [`android/tools/make-icons.ps1`](../android/tools/make-icons.ps1)（源：`src-tauri/icons/icon.png`） |

第三轮的 debug 包也同步重建为 0.1.7（1,320,760 B），不是早先的 0.1.0 旧产物。
0.1.7（含此前的 0.1.6）的源码改动仍在工作区，未提交、未推送。

协议没有改动大屏侧任何代码：手机端是浏览器扩展投屏的另一个实现，接收端仍是
`crates/horizon-cast/src/browser.rs` 的 9407 服务。

## 2. 环境

| 组件 | 版本/位置 |
| --- | --- |
| JDK | 17.0.21（`C:\Program Files\Microsoft\jdk-17.0.21.21-hotspot`） |
| Android SDK | platform 35 / build-tools 35.0.0 / platform-tools 37.0.1（`D:\AndroidSdk`） |
| Gradle | 8.9（`D:\gradle-dist`，仓库同时生成 wrapper） |
| AGP / Kotlin | 8.7.3 / 2.0.21，minSdk 26，targetSdk 35 |
| 手机 | HONOR ALI-AN00（X50），Android 15 / API 35，1200×2652 @520 dpi |
| 连接方式 | 无线 ADB `192.168.0.37:40157`，与电脑同 Wi-Fi |
| 接收端 | 192.168.0.27:9407，名称「戎耀会」，投屏码 `504282` |

环境坑（已写进 `android/README.md`）：`services.gradle.org` 在本机被阻断，Gradle 发行包
改从腾讯镜像下载；用户名含中文时测试 worker 的 `@argfile` 解析失败，`GRADLE_USER_HOME`
指到 `D:\gradle-home` 后正常。这只影响测试 worker，不影响 APK 构建。

## 3. 单元测试（不需要设备）

```
./gradlew testDebugUnitTest
BUILD SUCCESSFUL — 59 tests, 0 failures, 3 skipped
```

| 用例 | 数量 | 断言重点 |
| --- | --- | --- |
| `WireTest` | 2 | 24 字节小端头逐字段、1920×1080、长度、载荷位置；音频采样率/声道 |
| `AnnexBTest` | 5 | Annex B 直通、3 字节 start code、长度前缀转换、截断保护、SPS 补 start code |
| `WsClientTest` | 4 | 请求行带 `?token`、客户端帧掩码、文本/二进制往返、`{"type":"keyframe"}`、close 帧 |
| `DiscoveryTest` | 9 | `/info` 正常/404/异类服务/500/端口关闭、排序与候选集、`hub` 字段有/无/legacy |
| `DlnaTest` | 8 | SSDP 搜索报文、LOCATION 解析、AVTransport SOAP 报文、错误分支 |
| `MediaServerTest` | 9 | 媒体列表、Range 请求、MIME 判定、路径保护 |
| `HubClientTest` | 3 | 7878 文件服务握手、列表、上传请求体 |
| `HubMediaClientTest` | 14 | 片库/队列解析、setQueue/enqueue、queueMove、按 id 操作、暂停与 seek、上传 multipart 逐字节、错误文案（见 §11.3） |
| `RetryPolicyTest` | 2 | 退避序列 1/2/4/8/8 s、上限 5 次 |
| `ReceiverLiveTest` | 2 | 需要 `-PhorizonToken`，本轮跳过（见表下说明） |
| `HubMediaLiveTest` | 1 | 需要 `-PhorizonHub`，默认跳过；对真实主机跑完整投放流程（见 §11.4） |

`ReceiverLiveTest` 是对真实接收端发真实 H.264 裸流的用例，默认跳过；第一轮已用它
对 9407 服务验证过传输（180 帧 / 6 关键帧 / 2.6 MB，见第 5 节）。

## 4. 版本演进与修复

用户安装 0.1.1 后反馈「投屏通过，但速度比较慢，有些卡」，之后的每一版都对应一个
可复现的缺陷或体验问题：

| 版本 | 修复 | 验证方式 |
| --- | --- | --- |
| 0.1.2 | 传输页发送文本时崩溃（`Action.Builder` 图标参数传 `null`） | 真机复测不再退出；exit-info 不再新增 APP CRASH |
| 0.1.3 | 媒体服务（MediaServer）重置时崩溃 | 同上 |
| 0.1.4 | WebSocket 保活与断线重连：3 s Ping + TCP keepalive，退避 1/2/4/8/8 s，最多 5 次 | 15:12 停止接收端后自动恢复（见 6.2） |
| 0.1.5 | 断线时不再释放 VirtualDisplay（保留 MediaProjection 令牌），只重建编解码器，复用 `setSurface()` + `resize()` | 重连后直接出画面、不再弹二次授权；旋转原地重建（见 6.3） |
| 0.1.5 | 去掉 `welcome` 分支里重复的「正在投屏」日志 | 诊断日志每次连接只有一条 |
| 0.1.5 | 竖屏锁定（`screenOrientation="portrait"`），避免 `configChanges` 下横屏内容被导航栏裁切 | 横屏布局正常；物理旋转仍能改变采集画面 |
| 0.1.6 | 四页外壳（连接 / 传输 / 媒体 / 我的）+ 自研设计系统，媒体页内置 DLNA 控制器 | 42 项单测全绿；JVM 协议栈走通（见 §5） |
| 0.1.6 | 应用图标与 Windows 端统一 | 三套资源与 `src-tauri/icons/icon.png` 同图，见 §4.1 |
| 0.1.7 | 媒体页接上大屏片库与播放队列：多选上传、片库增删、队列排序/跳转/传输；主机端口取自 `/info` | 59 项单测（含 14 项 HTTP 客户端、2 项发现）；对真实主机跑通全流程，见 §11 |

版本号现状：`versionCode 8` / `versionName "0.1.7"`（`android/app/build.gradle.kts:14`）。

### 4.1 图标统一（0.1.6）

原来的 Android 图标是一张独立画的 `drawable/ic_launcher.xml` 矢量图，和桌面端不是同一个
图形。本轮把它换成「就是 Windows 那张」：`android/tools/make-icons.ps1` 以
`src-tauri/icons/icon.png` 为唯一源，产出三套资源——

| 资源 | 尺寸 | 画法 |
| --- | --- | --- |
| `mipmap-{m,h,x,xx,xxx}hdpi/ic_launcher.png` | 48/72/96/144/192 | 整张源图等比铺满，圆角由图自带，不做二次加工 |
| `mipmap-*/ic_launcher_round.png` | 同上 | 先以 artwork 上边缘像素色 `#08101F` 铺底，再用圆形裁剪叠上去，避免任何遮罩露白边 |
| `mipmap-*/ic_launcher_foreground.png` | 108/162/216/324/432 | 同一 `#08101F` 铺满后整幅贴图；`values/ic_launcher_background.xml` 亦为该色，`mipmap-anydpi-v26/ic_launcher.xml` 组 adaptive-icon |

验证：把 Windows `icon.png` 与三张 xxxhdpi 资源并排渲染成
`docs/shots/android-icon-parity.png`，四格图形、配色、圆角一致——即桌面与手机是同一张
图标，而不是「风格相近的两张」。

已知限制：adaptive foreground 是整幅铺满而非缩进 66dp 安全区，所以启用蒙版的启动器
（Pixel Launcher 一类）会把圆角方形的四条边裁掉一部分；荣耀 MagicOS 走 legacy mipmap
+ 自家形状遮罩，本轮未能装到真机上确认（ADB 端口已失效，见 §9）。

## 5. 第一轮：真实接收端 + JVM 协议栈（无手机）

启动器经 CDP 打开「投屏 → 接收 → 浏览器投屏」，接收端在 `127.0.0.1:9407` 与
`192.168.0.27:9407` 同时应答：

```json
{"name":"戎耀会","paired":true,"path":"/stream","port":9407,"service":"horizon-browser"}
```

用 `ReceiverLiveTest` 以**与 APK 相同的 `Wire`/`WsClient` 代码**投一段真实 H.264 裸流：

```
./gradlew testDebugUnitTest -PhorizonToken=504282 -PhorizonH264=/tmp/horizon-test.h264
live stream sent: 180 frames (6 keyframes) to 127.0.0.1:9407
```

大屏侧实时读数（CDP 抓取，非源码推断）：

| 观察点 | 结果 |
| --- | --- |
| 画面事实行 | `1280×720 · 180 帧 · 6 关键帧` |
| 计数条 | `180 帧 / 2.6 MB 视频 / 0 B 音频`（源文件 2,675,181 B） |
| 预览画布像素 | 425×239，14 种颜色分桶，均值 (124,133,123)——不是空帧 |
| 错误投屏码 | `000000` 时客户端报「投屏码不正确」（服务端 403） |
| 会话结束 | 流发完主动关闭后 `/info` 仍在线，面板回到等待连接状态 |

这一轮证明的是协议与传输，用的是 JVM 侧同源代码，不是手机编码器产物。

## 6. 第二轮：真机全链路

### 6.1 发现与首连

App 启动后 `/24` 扫描发现接收端，输入投屏码 `504282` 开始投屏。接收端读数
（`receiver_status`，CDP 抓取）：

```
connected:true  peer:192.168.0.37:48484
578×1280  keyframes:42  audioBytes:0
```

画面出现在大屏；`audioBytes` 为 0 是已知边界（本版不发音频）。
当前一次会话从 15:18:46 开始，显示「578×1280 @60fps 5326 kbps · GL 限帧」。

### 6.2 断线重连（关键节点）

在投屏过程中停止接收端再启动，诊断日志按时间给出完整状态机：

```
15:12:39 接收端已断开
15:12:39 连接中断，正在重连 · 192.168.0.27:9407
15:12:45 连接中断，重连中（1/5）
15:12:51 已重新连接 192.168.0.27:9407
15:12:51 正在投屏 · 192.168.0.27:9407
```

- 恢复用时 12 s，在 5 次退避（1/2/4/8/8 s，上限 30 s）之内；
- 全程没有重新弹 MediaProjection 授权，也没有「编码器启动失败」——这正是 0.1.5
  保留投影令牌、只重建编解码器的目的；
- 重连后接收端继续计数，画面恢复。

这一步也是内存「接收端断开即退投屏」的旧行为的对照：旧版直接停在断开原因，
必须手动再点「开始投屏」。

### 6.3 旋转

同一会话、同一 peer `48934` 上把手机横过来，接收端从 `578×1280` 切到 `1280×578`，
不需要重启投屏。实现上编码器用 `setSurface()` + `resize()` 复用同一个 VirtualDisplay，
而不是新建——新建会触发第二次 `createVirtualDisplay` 并抛出令牌失效异常（第一轮踩过）。

随后加了竖屏锁定：`configChanges` 阻止了 Activity 重建，但内容不会跟着窗口重新布局，
横屏时页面底部被导航栏裁掉；锁定竖屏后布局稳定，物理旋转仍然改变采集画面。

### 6.4 长时间空闲与保活

投屏保持在线 ≥ 23 分钟（15:18:46 开始，15:42:07 仍在投屏，App 进程无重启），
其间手机有数分钟停在静态页面：

| 时间 | receiver frames | videoBytes | connected |
| --- | --- | --- | --- |
| 15:29 | 7,631 | 73.1 MB | true |
| 15:32 | 7,637 | 76.8 MB | true |
| 15:42 | 8,588 | 88.4 MB | true |

静态页面上帧计数几乎不动（7631 → 7637，约 3 分钟 6 帧），但连接一直保持——这是
3 s Ping + TCP keepalive 在起作用：没有帧可发时链路也不会被中间设备回收。

### 6.5 崩溃面

`dumpsys activity exit-info` 的时间线（新→旧）：

```
15:15:09 PACKAGE UPDATED   ← 0.1.5 安装
15:09:18 PACKAGE UPDATED
15:00:09 PACKAGE UPDATED
14:57:41 APP CRASH(EXCEPTION)
14:51:27 USER REQUESTED (FORCE STOP)
14:45:00 APP CRASH(EXCEPTION)
14:43:15 APP CRASH(EXCEPTION)
14:30:01 PACKAGE UPDATED
14:25:15 APP CRASH(EXCEPTION)
```

0.1.5 安装（15:15:09）之后到 15:42 没有任何崩溃记录；此前的 4 次崩溃发生在调试
文本发送与媒体服务重置缺陷的阶段。需要说明：这台 HONOR 的 logcat 是空的
（`adb logcat -d | wc -l` = 0），所以取证只能靠 App 内诊断日志、`dumpsys` 和接收端计数。

## 7. 性能：把「卡」量化

### 7.1 720p60 连续运动

在手机播放连续动画的页面测量 19 s：

| 指标 | 数值 |
| --- | --- |
| 接收端摄入帧数 | 1,106 帧 / 19 s ≈ **58.2 fps** |
| 视频字节 | 12.47 MB ≈ **5.25 Mbps** |
| 发送端丢帧 | **0** |
| 分辨率 / 帧率 | 1280×720 @60 |

也就是说：60 fps 的源，接收端收进了 58.2 fps，丢帧为 0——**链路不是瓶颈**，局域网和
编码端都跟得上。

### 7.2 瓶颈在桌面解码

大屏 `MirrorPreview` 的 DEV 性能探针（生产构建不含）给出单帧耗时：

```
ipc 10.4 ms + down 10.3 ms ≈ 20.7 ms/frame  →  约 48 fps 上限
```

这台接收机是软件解码路径，48 fps 的上限对上 60 fps 的源，每 6 帧左右就会错过一帧，
观感就是「有些卡」。复现条件也吻合：手机停在静态页时画面完全正常，一动起来才抖。

### 7.3 结论与默认值

- 60 fps 在当前的桌面解码器上不可能稳定，30 fps 才是干净配置；
- `Prefs` 的默认值已经是 `maxEdge=1920` / `fps=30`（1080p30），30 fps 落在 48 fps
  上限之内，不存在追帧缺口；
- 设置页文案也写明了取舍：「60 fps 更耗带宽与电量，接收端解码跟不上时会反复重同步。」
- 若后续要跑 60 fps，需要改桌面端的解码/上屏路径（例如硬解或丢帧策略），
  而不是继续加码率——720p60 只用了 5.25 Mbps，带宽还有大量余量。

## 8. 交互缺陷与待办

### 8.1 输入框点击不弹键盘（未解决，已隔离）

现象（真机，0.1.5 之前与 0.1.5 均出现）：

- 连接页「投屏码」「手动地址」点上去不出现光标，也不弹键盘；
- 同时按钮、下拉刷新、页面切换都正常，说明触摸事件本身能到达 App；
- `dumpsys input_method` 里 `mInputShown=false`，被服务的 view 仍是一个 `TextView`；
- 点「开始投屏」会因为 `requestFocus()`（`pages/ConnectPage.kt:364`）间接让输入框获得焦点，
  键盘随之出现，所以投屏流程不被阻断。

代码侧已排除：`Ui.field`/`Ui.codeField` 是标准 `EditText`（`ui/Ui.kt:176`、`:195`），
没有被设为不可聚焦，外层是普通 `ScrollView`（`ui/Ui.kt:84`），App 里没有任何
`onInterceptTouchEvent` / `OnTouchListener` 会吞事件。

尚未定位根因。可能方向（均为待验证假设，不是结论）：窗口失焦时 IME 不接管
（Honor 投屏期间的系统悬浮指示）、或测试用的 `adb input tap` 命中位置与实际控件
存在偏移。**复测需要独占手机**：点输入框 → 看光标与 `dumpsys input_method` 的
`mServedView` 是否变成 `EditText` → 再点一次看键盘是否出现。手机在用户手上，
本轮没有继续驱动。

### 8.2 其他待办

- 音频仍未实现（`kind=2` 只保留在协议与 `Wire` 单测里），接收端显示 `0 B 音频`；
- 传输页（7878）与媒体页的 DLNA 直投本轮只有单元测试覆盖，没有做真机端到端；
  媒体页新增的片库/队列已对真实主机跑通客户端流程（§11.4），但手机界面本身未点亮；
  接收端 DLNA 面板当前显示 `media engine error 4`，是更早一次 JPG 投屏留下的状态，
  与 Android 发送端无关；
- 大屏「浏览器投屏」面板在 15:29 左右自行关闭过两次，重开后立即恢复，
  属于桌面端行为，本轮没有追查。

## 9. 没有覆盖到的（诚实边界）

- **端到端延迟未测**：原计划用手机秒表 + 大屏截图对比读数，但 15:28 手机被拿走后停在
  短信验证码页面，帧计数冻结在 7631，秒表方案中断。现有数字只有接收端单帧解码耗时
  （20.7 ms），不是手机屏幕到大屏显示的端到端延迟。
- **1080p30 没有单独长时间跑**：默认值成立，但「30 fps 落在解码上限内」是推理，
  不是本轮的实测结论。
- **60 fps 长跑未测**：只在 19 s 窗口内测过 720p60。
- **后台/锁屏投屏未测**：没有验证熄屏后前台服务是否继续采集（部分 OEM 会限制）。
- **重连上限行为未测**：只验证了 1 次重连成功，没有验证 5 次全部失败后的提示与收尾。
- **0.1.6 / 0.1.7 未装真机**：18:00 后手机侧无线调试端口失效（`192.168.0.37:40157` 与 5555 均
  `10061` 拒绝，`adb devices` 为空），所以 §4.1 的图标、四页外壳与新增片库区块的真机观感，
  以及 #103 输入框聚焦、#104 端到端延迟与 1080p30 长跑都还没在手机上确认过。
  这期间手机仍在同一局域网内（18:5x 它还以 DLNA 控制器向大屏投了一段 5:12 视频，
  见主验证记录 §8.2），说明是调试端口没开，不是设备离线。

## 10. 复现步骤

```bash
# 1. 构建
cd android
GRADLE_USER_HOME=/d/gradle-home ./gradlew assembleRelease

# 2. 单测（59 项，3 项 live 用例默认跳过）
GRADLE_USER_HOME=/d/gradle-home ./gradlew testDebugUnitTest

# 3. 对真实接收端（大屏已开「浏览器投屏」）
GRADLE_USER_HOME=/d/gradle-home ./gradlew testDebugUnitTest \
  -PhorizonToken=<6位投屏码> -PhorizonH264=/tmp/horizon-test.h264

# 3b. 对真实片库主机（大屏已启动 HORIZON，7878 在听）
GRADLE_USER_HOME=/d/gradle-home ./gradlew testDebugUnitTest \
  -PhorizonHub=192.168.0.27:7878 --tests 'com.horizon.castsender.HubMediaLiveTest'

# 4. 真机
adb install -r app/build/outputs/apk/release/app-release.apk
adb shell am start -n com.horizon.castsender/.MainActivity
adb shell dumpsys activity exit-info com.horizon.castsender   # 崩溃面
adb shell dumpsys input_method | grep -i served               # 输入法服务状态
```

接收端读数（大屏已开着调试口 9333）：

```bash
node tools/cdp-eval.mjs "window.__TAURI_INTERNALS__.invoke('receiver_status').then(r=>JSON.stringify(r.browser))"
```

## 11. 第三轮：片库与播放队列对齐（0.1.7）

这一轮把手机从「只能 DLNA 投一条」补到与浏览器扩展对等：手机媒体先进大屏
**片库**，可以多选一次投完，队列能排序、跳转、暂停/停止——手机、扩展、大屏
三个入口驱动的是同一条队列（`src-tauri/src/media_stage.rs` 的 `MediaStage::invoke`），
而不是三份各说各话的播放状态。

### 11.1 交付物与构建

| 内容 | 位置 / 数值 |
| --- | --- |
| 主机客户端 | `android/app/src/main/java/com/horizon/castsender/HubMediaClient.kt`（片库、队列、传输、multipart 上传） |
| 媒体页新增区块 | `android/app/src/main/java/com/horizon/castsender/pages/MediaHub.kt`（主机连接、多选投放、片库、队列） |
| 发现扩展 | `Discovery.kt` 的 `Receiver.hub`、`LanDiscovery.probeHub` |
| 客户端单测 | `android/app/src/test/java/com/horizon/castsender/HubMediaClientTest.kt`（14 项，原始 socket 假主机） |
| 真实主机联调 | `android/app/src/test/java/com/horizon/castsender/HubMediaLiveTest.kt`（`-PhorizonHub`，默认跳过） |
| Release APK（0.1.7 / versionCode 8） | `android/app/build/outputs/apk/release/app-release.apk`，1,005,389 B，sha256 `01903fc3…f84ec6f8` |
| Debug APK（同版本） | `android/app/build/outputs/apk/debug/app-debug.apk`，1,320,760 B，sha256 `52e3c99b…edbc3a2` |

```
aapt dump badging app-release.apk
package: name='com.horizon.castsender' versionCode='8' versionName='0.1.7' …
```

APK 内确有本轮新增的类（`classes.dex` 符号表）：

```
Lcom/horizon/castsender/HubMediaClient;
Lcom/horizon/castsender/pages/MediaHub;
```

### 11.2 主机端口来自 `/info`，不写死

9407 接收端的 `/info` 本来就带 `"hub":7878`（浏览器扩展靠它找片库）。手机端沿用：
`Discovery.probe` 解析出 `hub`，`LanDiscovery` 用**这个端口**探测片库服务
（`GET /` 看是否含 `snapdrop`），连接时也带上它；只有发现结果没有 `hub` 字段
（老版本接收端）时才退回 7878。单测覆盖：

| 用例 | 断言 |
| --- | --- |
| `identifiedReceiverReportsTheHubPortItAdvertises` | `hub:7878` 被读进 `Receiver.hub` |
| `receiverWithoutHubFieldLeavesTheLibraryUnknown` | 没有 `hub` 时为 `null`，交给默认值 |
| `missingInfoEndpointIsTreatedAsLegacyReceiver` | 404 的老接收端仍可投屏，且 `hub == null` |

### 11.3 客户端：拿原始 socket 假主机验线路形状

JVM 单测类路径上没有 `com.sun.net.httpserver`，所以 `FakeHub` 直接用
`ServerSocket` 收一个请求回一个响应；multipart 部分按字节解析，和扩展的
`FormData` 产物对齐。重点断言：

| 用例 | 锁住的东西 |
| --- | --- |
| `libraryKeepsChineseTitlesAndByteCounts` | `title`/`bytes`/`kind` 原样解析，中文不丢 |
| `queueReadsItemsAndThePlayingState` | 条目、`current`、`playing`、`state.uri/position/duration`、`halted == null` |
| `haltedQueueKeepsTheReason` | 主机自行停止时的原因与 `current: null` → `-1` |
| `setQueuePostsEverySelectedIdInOneRequest` | 一次 POST 带全部 id，`autoplay=true` |
| `enqueueAppendsWithoutTakingTheScreen` | 追加不自动播放（`autoplay=false`） |
| `queueMoveSendsTheSlotItCameFromAndTheSlotItGoesTo` | 报文为 `{index,to}`，正是主机的槽位语义 |
| `queueRemoveAndSetNextWorkById` | 按 **id** 而不是下标操作 |
| `transportReadsTheQueueNestedInsideTheStatus` | `transport` 回的是 `{state,queue}`，客户端要拆 `queue` |
| `seekCarriesThePositionItWasGiven` | `seek` 带 `at` |
| `aRefusedActionCarriesTheHubsOwnMessage` | 400 的 `error` 原样抛给 UI |
| `aClosedPortIsReportedInsteadOfHanging` | 连接被拒 → 带地址的「无法连接」 |
| `uploadSendsTheWholeSelectionInOneMultipartRequest` | 两个文件、中文名、64 B + 200,000 B 逐字节一致；进度按**载荷**累计（末值 = 200,064） |
| `aRejectedUploadExplainsWhy` | 400 `文件太大` 透传 |
| `anUploadTheLibraryDroppedIsNotReportedAsSuccess` | `items` 为空 → 「片库没有接受这些文件」，不假装成功 |

上传是按精确 `Content-Length` 的一次流式请求：发送端先算好头部与载荷总长，
进度条只统计载荷字节；提供方报不出大小的文件先复制到缓存再传。主机侧对
0 字节字段直接跳过，这一点与扩展共用同一段服务端代码。

### 11.4 真实主机联调（`HubMediaLiveTest`）

环境：大屏（192.168.0.27）上 HORIZON 主程序在跑，7878、8290、9407 同时在线；
片库已有 1 条用户素材。用**与 APK 相同的客户端代码**跑完整流程：

```
./gradlew testDebugUnitTest -PhorizonHub=192.168.0.27:7878 \
  --tests 'com.horizon.castsender.HubMediaLiveTest'
```

```
[live] hub 192.168.0.27:7878 library baseline=1
[live] upload items=[f28088f4c22c:audio:192044, 4dd03899e192:image:70]
[live] setQueue phase=playing position=0.0987708
[live] transport pause→play ok
[live] image phase=playing duration=0.0
[live] queueMove order=[audio, image]
[live] queueRemove then stop ok
[live] cleanup library=1 queue=0
```

逐步对应关系：

| 步骤 | 验证的事实 |
| --- | --- |
| 上传 | 12 s 8 kHz WAV（192,044 B）+ 1×1 PNG（70 B）在**一个**请求里进片库；`bytes`、`source`（`image/png`）、`file` 前缀都对；进度到 100 |
| setQueue | 两条同时入队，`current=0`，地址形如 `/media/<id>/<文件名>`——大屏播放器据此识别媒体类型 |
| 播放态 | 等 `state.uri` 真的指向 WAV 且 `phase=playing`，位置从 0.098 s 开始 |
| 暂停/继续 | `pause` 后先确认队列 `playing=false`，再轮询到 `phase=paused`；`play` 后回到 `playing` |
| 单张图片 | 只投图片同样进入 `playing`（静帧靠停留时间前进），这正是曾经触发 `SRC_NOT_SUPPORTED` 的地址形态 |
| 追加 | `enqueue` 后仍是图片在播，不被接管 |
| 排序/跳转 | `queueMove(1,0)` 后 WAV 到第 0 位；`jump(1)` 切回图片 |
| 删除当前项 | 图片被移除后队列自动接着播 WAV，随后 `stop` 落到 `stopped` 且 `playing=false` |
| 清理 | 删除两项后片库回到 1 条、队列 0 条；`finally` 里再兜底按前缀清一次 |

注意：这条用例会**接管大屏当前的播放**（先用 `setQueue` 覆盖队列，结束时 `stop`），
只适合在可以清空队列的联调环境里跑；它删的只有自己上传的两个 id。

两个实测坑，都已修进测试而不是放宽断言：

1. **`transport` 的回执是滞后的**：原生播放器在自己的线程上执行命令，动词返回的
   快照可能还是上一状态。所以「暂停成功」不看回执，而是像手机 UI 一样 1 s 轮询
   到 `paused`。
2. **`state.uri` 落后一个加载**：第一版只看 `phase=playing`，结果在 12 s 的 WAV 上
   读到 `position=34.883` —— 那是**上一条**（用户片库里正在播的图片）的读数。现在
   `MediaQueue` 暴露 `state.uri`，等待条件改为「phase 为 playing 且 uri 指向本条目」，
   重跑后位置是 0.098 s。单测也补了 `queue.uri` 的断言。

### 11.5 手机界面（代码就位，未在手机上点过）

媒体页现在从上到下是：标题 → **大屏片库 · 播放队列** → **DLNA 直投**。

- 片库主机：扫描到的 HORIZON 主机会列出，点一行即连；连上后显示
  `ip:端口 · 片库与队列`，可「更换主机」；地址栏保留上次连接，下次进页静默重连。
- 投放手机媒体：「选择媒体（可多选）」一次挑多个视频/音乐/图片，逐个可移除；
  「播放所选」把整批设为队列并开播，「追加到队列」排到队尾且不打断当前播放；
  上传时显示百分比与已传/总量。
- 片库：条数、每项类型与大小；`▶` 单条投、`＋` 追加、`✕` 弹确认框后从电脑片库删除
  （队列里的同一条随之消失）。
- 播放队列：当前条目与状态（含「加载中/已暂停/播完/播放出错」中文）、播放/暂停/停止；
  每行 `↑` `↓` 排序、`⇥` 设为下一条、`✕` 移除，点行即跳转；页面可见时 1 s 轮询一次，
  与扩展一致。

**诚实边界**：这一轮没有手机可用——`adb devices` 为空、`emulator -list-avds`
没有 AVD，所以以上界面**没有在真机上点过**，多选器、进度条、弹窗和排序按钮只有
代码与编译产物为证；§8.1 的「输入框不弹键盘」若仍存在，主机的地址输入框会受影响
（扫描列表点击不经过输入框）。真机 E2E 仍是待办，不能把本节写成「已验证」。

### 11.6 本轮没做到的

- **真机 UI E2E**：同上，没有设备/模拟器；界面观感与触摸行为未验证。
- **多选器的边界**：不同 ROM 的 `ACTION_OPEN_DOCUMENT` 是否会一次返回多个 URI、
  以及「大小未知」的提供方走复制缓存这条路径，都没有真机样本。
- **大文件与断网**：上传是单次定长请求，没有断点续传；中途断网只会报错，需要用户
  重选。GB 级视频没有实测。
- **主机鉴权**：7878 与扩展共用同一套无鉴权接口，同一局域网内任何设备都能操作队列；
  这是现状而不是本轮新增的限制。
- **息屏/后台轮询**：队列刷新只在前台页面 tick 时进行，后台行为未测。

## 12. 第四轮：真机 E2E（0.1.7，无线 adb）

§11.5 留下的「界面没在真机上点过」在这一轮补上了：Honor ALI-AN00 通过无线 adb
连上（`192.168.0.37:43105`，本轮给的配对码 `695056`/端口 `42087` 没有用上——设备
早已配对成功，`adb devices` 直接可写），主机是同一台电脑的 `192.168.0.27:7878`。
点按坐标全部由截图（1200×2652）量出，没有使用无障碍树。

### 12.1 两个真机上才暴露的缺陷

**A. 片库「选择媒体」连接后仍是禁用态**

复现：媒体页连上主机后，「选择媒体」看起来是浅蓝（`#EAF2FE` 的 45% 叠加），
点击无反应，多选器不弹出。

根因：`MediaHub.renderPending()` 负责 `pickButton.isEnabled = client != null && !busy`
和 `alpha`，但它只在页面切换、传输进度等路径被调用；**连接成功与断开这两条路径漏了**，
按钮停留在建连前的禁用态。

修复（`pages/MediaHub.kt`）：连接成功的回调里补 `renderPending()`（第 366 行附近），
断开路径改为 `renderHub(); renderPending(); renderLibrary(); renderQueue()`（第 381 行附近）。
重装后按钮恢复主色、点击弹出系统多选器。

**B. 走完系统选择器回来，传输通道已经断了**

复现（修复前）：传输页 →连接 →「选择文件发送」→ 在系统选择器里停留 → 选文件返回，
界面弹 `发送失败 · horizon-transfer-test.txt` 和 `未连接`，文件没有落到电脑。
用 Home 键压后台再回来同样必现。

根因：传输用的是 `HubClient`/`WsClient` 的 WebSocket（`ws://host:7878/server/webrtc`），
它只有 TCP keepAlive、没有周期 ping；Activity 被系统选择器（或 Home）暂停后，这条
连接在服务端被回收，而 `TransferPage` 没有任何重连入口——`onActivityResult` 直接
`client!!.sendFile(...)`。

修复：

| 位置 | 改动 |
| --- | --- |
| `pages/Page.kt` | 新增 `open fun onActivityResume()` 生命周期钩子 |
| `MainActivity.kt:56` | `onResume()` 里 `pages.forEach { it.onActivityResume() }` |
| `pages/TransferPage.kt:60` | `wantConnection` 记住「用户想连着」，`afterConnect` 暂存待执行动作 |
| `pages/TransferPage.kt:382` | `ensureConnected { sendFiles(uris) }`：已连就直接发，断了先静默重连再发 |
| `pages/TransferPage.kt:399` | `onActivityResume()`：清掉死连接；若用户仍想连着就静默重连 |

`connectTo(address, quiet = true)` 在静默模式下吞掉所有 toast，避免重连时刷屏；
连接成功后统一走 `runAfterConnect()` 执行暂存动作，用户不需要再点一次「发送」。

### 12.2 修复后的端到端证据

**文件传输（本轮重点）**

```
adb push D:/tmp/horizon-transfer-test.txt /sdcard/Download/      # 10:01，48 B
```

手机：传输 → 连接（`192.168.0.27:7878`，徽标变「已连接」）→ 选择文件发送 →
系统选择器「下载内容」→ 搜索 `horizon-transfer` → 点 `horizon-transfer-test.txt`。
选择器从 10:08 开到 10:12，回来时传输通道已经断过一次，界面没有报错，
直接显示：

```
发送完成 · 1 个文件
horizon-transfer-test.txt
已保存到电脑的接收文件夹
```

电脑端落盘与内容一致：

```
-rw-r--r-- 1 戎耀会 197121 48 Oct 31 10:12 horizon-transfer-test.txt
HORIZON transfer test 2026-10-31T10:01:40+08:00
md5  9cc362daed92c7704a8582b10b60738d   # 源文件与落盘文件相同
```

**文字发送**：传输页输入一行文字 → 发送，电脑接收目录出现 `收到的文字.txt`
（一行，主机时间戳 `[1793412267]` + 测试串），传输记录里同时出现
「文字: …」条目，主机地址为 `192.168.0.27:7878`。

**媒体片库与队列**：媒体页多选一段手机本地视频（52,791,537 B）→ 上传百分比走到 100 →
「播放所选」让大屏片库新增一条并开播（`POST /api/media/setQueue`，`autoplay=true`）；
随后在片库行点 `＋` 追加另一条，`GET /api/media/queue` 读到
`items=[1d471d45a590, 12db0c3088f1]`、`current=0` 正在播，界面提示「已追加：…」。
两条指令都走 `media_stage.rs` 的同一条队列，与扩展、大屏一致。

**我的页**：版本 0.1.7、720p、60fps、HONOR ALI-AN00、`192.168.0.37`、Android 15 API 35。

### 12.3 诚实边界

- **点按是坐标不是控件**：这台机器上 `uiautomator dump` 一直报 "could not get idle
  state"（雷达动画），只能用截图量坐标；系统选择器里出现过一次点到相邻行的误触
  （打开了 `MFiles` 目录），以及搜索结果行在输入法未收起时点不动。最终路径可用，
  但这类坐标脚本不如控件选择稳。
- **重连是行为证据**：修复后的成功发送发生在选择器停留约 4 分钟之后，说明通道确实
  死过又重连成功；但没有留下 socket 层的日志时间线，「重连发生在哪一秒」没有直接证据。
- **投屏未复测**：本轮没有重跑镜像（顶栏一直是「待连接」），§7 的性能数据仍是上一轮
  的测量结果。
- **0.1.7 的 release APK 没有重新打包**：本轮只装了 debug 包做验证，§11.1 里的
  release APK 仍是修复前构建的，发版前需要重新 `assembleRelease` 并复核 sha256。

### 12.4 环境复位（本轮做了什么、没做回什么）

- 动画缩放：为方便点按曾把 `animator/transition/window_animation_scale` 置 0，
  已全部恢复 `1.0`。
- 测试文件：删除了电脑接收目录里的 `horizon-transfer-test.txt`、`收到的文字.txt`
  和手机 `/sdcard/Download/horizon-transfer-test.txt`。
- **片库在 10:04 被外部清空**：`media/index.json` 只剩 `{}`、媒体目录为空，
  本轮上传的两条测试条目（`1d471d45a590`、`12db0c3088f1`）与原先队列里的
  `0f14dd6df15d` 都已不在，队列因此停在空队列 + `stopped`。这不是本轮脚本的删除
  动作（脚本只在 §11.4 删过自己上传的 id），也无法据此恢复原队列——记录在此而不是
  假装复位。
- `哪吒入心之途.mp4`（81,609,436 B，10:05 落盘）疑似使用者自己的传输，**保留未动**。

