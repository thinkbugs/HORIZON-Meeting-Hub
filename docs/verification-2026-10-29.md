# 验证记录 2026-10-29 · 侧边栏闪烁 / 卡顿 / 批注配置 / 镜像延迟 / 投屏码与 Miracast 入口 / 欢迎应用图片与音乐

验证对象：`D:\codeX\meeting\target\release\horizon-launcher.exe`（v0.5.1，`npx tauri build` 产出，安装包 `target\release\bundle\nsis\HORIZON Meeting_0.5.1_x64-setup.exe`）

验证方式：WebView2 远程调试端口（`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333`）+ CDP 脚本读取真实 DOM、注入指针事件、采样 `requestAnimationFrame` 帧间隔、`Page.captureScreenshot` 截图。不依赖屏幕坐标点击。

## 1. 侧边栏闪烁

- `.sidebar` 节点数量：1；启动后 3 秒内 `MutationObserver` 记录到 0 次节点增删与 class/style 变更，节点引用不变。
- 根因已移除：前端不再调用 `create_sidebar_window`，Rust 侧 3 次重试创建第二个 WebView2 窗口的路径不再被触发（`tauri.conf.json` 中只有 `main` 一个窗口）。
- 拖拽吸附：拖到右缘释放 → `sidebar--right`，`left=1790`；拖到空白处释放 → `sidebar--custom`，位置写入配置；重新拖到右缘 → 再次回到 `sidebar--right`。
- 位置持久化：`%APPDATA%\com.horizon.meeting\launcher-state.json` → `settings.sidebarPosition = "right"`。

## 2. 卡顿

帧间隔采样（120 帧，窗口可见且聚焦）：

| 场景 | 中位 | p95 | 最大 | >32ms 帧 |
| --- | --- | --- | --- | --- |
| 首页静置 | 16.7ms | 16.8ms | 16.9ms | 0 |
| 拖拽侧边栏（45 次移动，约 1.9s） | 16.7ms | 16.8ms | 16.9ms | 0 |

改动项：

- `.sidebar.is-dragging` 关闭 `backdrop-filter` —— 拖拽时每帧重算背景模糊是主要卡顿源。
- 移除全屏级 `filter: blur()`：`.ambient__nebula`(64px)、`.ambient__beam`(28px)、`.ambient__earth-clouds`(20px)。这些图层原本就是大跨度径向渐变，去掉模糊后截图对比无可见差异。
- 移除 `.ambient__earth::before` 的 `mix-blend-mode: screen`（230vw 宽元素使用混合模式会强制整屏回读）。
- 遮罩模糊：`.overlay` 22px → 10px，`.overlay--drawer` 18px → 9px，并提高底色不透明度保持观感。
- `receiver_status` 每秒轮询后按 JSON 签名比对，数据未变不再 `setState`，避免整棵树每秒重渲染。

## 3. 批注配置

- 设置抽屉章节顺序：大屏信息 / 桌面壁纸 / 首页快捷方式 / 应用库 / 工具侧边栏 / **批注应用** / 系统行为。
- 「批注应用」章节含「重新检测」「浏览本机应用」两个按钮；本机自动检测到 `Ink Canvas Ultra`（`C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Ink Canvas Ultra.lnk`），来自开始菜单快捷方式扫描。
- 选中后显示「当前首选：Ink Canvas Ultra」，写入 `launcher-state.json` 的 `preferredAnnotationApp` / `preferredAnnotationPath`。
- 点击侧边栏「批注」→ 提示「已启动 Ink Canvas Ultra」，外部进程确实启动（验证后已关闭）。

### 顺带修复的缺陷

- `launchEntry` 的 `useCallback` 依赖漏了 `launchAnnotation`，导致点击「批注」永远走到「尚未配置批注应用」分支（闭包里是初始空配置）。已补依赖。
- `sidebar-open-annotation` 事件监听器改用 ref 转发最新回调，避免长生命周期闭包读到旧配置。
- 关于页版本号 `v0.4.0` → `v0.5.1`。

## 4. Office 改为用户自选应用

取消 `msoffice` / `wps` 两选一的预定义，改为列出本机实际安装的办公应用，点哪个启动哪个，并把选择记为默认。

- 设置字段：`preferredOfficeApp` / `preferredOfficePath`（取代 `preferredOfficeSuite`）。
- 新增 Rust 命令 `detect_office_apps`：先查标准安装路径（WINWORD/EXCEL/POWERPNT、WPS 的 ksolaunch/wps/et/wpp，含 `%LOCALAPPDATA%` 用户级安装、LibreOffice、OnlyOffice），再扫描开始菜单和桌面（`%USERPROFILE%\Desktop`、`%PUBLIC%\Desktop`）里的 `.lnk`；按显示名去重，避免同一应用的多份快捷方式重复出现。
- 面板：`重新检测` / `浏览本机应用`（原生文件选择）× 应用列表，点击即启动；当前默认项标「默认」。（面板的入口与默认应用逻辑在第 5 节调整。）
- 移除了 Rust 里 `openBuiltin("office")` 的硬编码启动逻辑。

验证结果（本机）：

- 检测到 `WPS Office`（`%APPDATA%\...\Start Menu\Programs\WPS Office.lnk`）；桌面同名快捷方式被正确去重，只显示一条。
- 点击该卡片 → 提示「已启动 WPS Office」，WPS 进程确实启动（验证后已关闭）。
- 首选写入 `launcher-state.json`：`preferredOfficeApp = "WPS Office"`、`preferredOfficePath = ...WPS Office.lnk`。
- 顺带：桌面扫描让批注列表多出 `智绘教(屏幕批注工具)`（`%USERPROFILE%\Desktop\*.lnk`）。

## 5. Office 默认应用直启

问题：面板里没有「设为默认」的入口；而且即使选过应用，点击「Office」仍然每次都弹选择面板。

调整后的逻辑：

- 已设置默认应用 → 点击 Office 直接启动，提示「已启动 X」，不再弹面板。
- 未设置默认应用 → 打开面板作为首次选择引导（文案「选择一个常用的办公应用，之后点击「办公应用」会直接启动它。」，底部「尚未设置默认办公应用」）。
- 默认应用启动失败（快捷方式/路径失效）→ 提示错误并自动打开面板，否则用户每次点击都会失败且没有出路。
- 面板每行拆成「启动区 + 设为默认」两块：「设为默认」只写配置、不启动，提示「已将 X 设为默认办公应用」，该行显示「默认」徽标；点击行主体仍然启动并同时记为默认。
- 设置抽屉新增「办公应用」章节，与「批注应用」共用抽出的 `ApplicationPickerSection` 组件：重新检测 / 浏览本机应用 / 点行设为默认 / 清除选择。日常换默认应用走设置，面板只承担首次选择与失效兜底。
- `launchEntry` 的依赖数组补上 `launchOffice`（与上一轮批注的闭包缺陷同类，避免读到旧的空配置）。

验证（CDP，`target\release\horizon-launcher.exe`）：

| 步骤 | 结果 |
| --- | --- |
| 设置 → 办公应用 → 清除选择 | 「默认应用」条消失，列表取消选中 |
| 无默认时点击 Office | 面板打开，`beforeDefault: 尚未设置默认办公应用`，行内出现「设为默认」 |
| 点「设为默认」 | 徽标「默认」，底部「默认应用：WPS Office」，toast「已将 WPS Office 设为默认办公应用」，面板保持打开 |
| 关闭面板再点 Office | toast「已启动 WPS Office」，`panelAfterLaunch: false` |
| 批注应用回归（共用组件后） | 点第一行 → 选中态与「当前首选」同步切换到 `Ink Canvas Ultra` |

未改动：`detect_office_apps` 的检测逻辑与 `preferredOfficeApp` / `preferredOfficePath` 字段格式。

## 6. 侧边栏点击无响应

现象：侧边栏 7 个工具用鼠标点击全部没有反应。

根因：`Sidebar` 的拖拽逻辑在 `pointerdown` 时立刻对容器调用 `setPointerCapture`。指针被容器捕获后，`pointerup` 的目标变成 `.sidebar` 容器，浏览器随之把 `click` 也派发到容器，按钮的 `onClick` 永远收不到事件。用 DOM `.click()`（跳过 pointer 序列）测试时一切正常，所以之前几轮验证没暴露出来。

修复（`src/components/Sidebar.tsx`）：

- `pointerdown` 只记录起点，不捕获指针。
- 移动超过 3px 判定为拖拽时才 `setPointerCapture`，把后续 `pointerup`/`click` 留在容器上，避免拖完误触按钮。
- 位移不足 3px 的按下-抬起视为点击，事件正常落到工具按钮。

验证（`Input.dispatchMouseEvent` 真实鼠标序列，即 pointerdown → pointerup → click 全链路）：

| 操作 | 修复前 | 修复后 |
| --- | --- | --- |
| 点「文件传送」 | 无任何反应，`click` 目标是 `.sidebar` | 打开文件传送面板，`click` 目标是按钮内图标 |
| 点「欢迎」 | 无反应 | 打开欢迎页（`welcome-root`） |
| 点「记事本」 | 无反应 | 新起 `notepad.exe`（验证后已关闭该实例） |
| 拖动侧边栏 | 整条可拖 | 仍可拖到 `left=1470,top=400`（`sidebar--custom`），且拖拽结束不会误启动工具 |

## 7. AirPlay 发送端找不到接收端

现象：iPhone / iPad / Mac 的屏幕镜像列表里看不到这台机器；同一网络下浏览器投屏正常（两条链路没有关系）。

根因：mDNS 应答里的实例名没有带服务类型。抓包（`_airplay._tcp.local` PTR 查询，从 5353 发出）看到的是：

```
PTR  _airplay._tcp.local -> HORIZON 86
SRV  HORIZON 86 -> HORIZON-86.local:5000
TXT  HORIZON 86 -> deviceid=12:58:2f:7f:27:64 | ...
```

同网段一台真实的 macOS AirPlay 接收端（iMacX，192.168.0.122）同一条查询的应答是：

```
PTR  _airplay._tcp.local -> iMacX._airplay._tcp.local
SRV  iMacX._airplay._tcp.local -> iMacX.local:7000
PTR  _raop._tcp.local    -> 46A1ECA53641@iMacX._raop._tcp.local
```

Apple 的解析器把 PTR 的目标名当作全限定名逐字解析，不会再补 `._airplay._tcp.local`。我们给出的是裸实例名 `HORIZON 86`，它拿到后就查 SRV/TXT，永远查不到，于是设备在列表里根本不出现。自家扫描之所以一直能列出 HORIZON，是因为 `MdnsBrowser` 按 PTR 的第一个标签建索引、再用同一个键去拼 SRV/TXT，把广播端的错误容忍掉了，所以前面的自测没能暴露。

修复（`crates/horizon-cast/src/airplay/mdns.rs`）：

- PTR 的 rdata、SRV/TXT 的 owner 一律写成全限定实例名：`<MAC>@<名称>._raop._tcp.local` 与 `<名称>._airplay._tcp.local`。
- 实例名按「一个标签 + `_raop`/`_tcp`/`local` 三个标签」逐段写入，机名里带点也不会被拆错。
- 广播改成 AirPlay 1 的写法：`model` / `am` 用 `AppleTV3,2`（`/info` 同步改），不发布 `protovers`，`_airplay` TXT 补 `acl=0`。之前在只实现 AirPlay 1 的情况下冒用 `AppleTV5,3`，发送端可能按 AirPlay 2 的流程来协商。
- `matches()` 同时接受全限定名和裸实例名，避免解析器缓存了旧名字后收不到应答。

验证：

| 检查项 | 结果 |
| --- | --- |
| 单元测试 `instance_names_on_the_wire_are_fully_qualified` 等 airplay::mdns 全部 | 10 passed |
| `cargo test -p horizon-cast` | 181 passed / 4 ignored |
| 线上抓包（修复后） | `HORIZON 86._airplay._tcp.local`、`12582F7F2764@HORIZON 86._raop._tcp.local`，与 iMacX 写法一致 |
| 严格解析（PTR 目标逐字查 SRV/TXT/A，不补后缀） | 两个服务全部 PASS，A 记录 192.168.0.52 |
| 应用内「局域网中的投屏设备」重新扫描 | HORIZON 86 192.168.0.52:5000 与 iMacX 192.168.0.122:7000 均在列 |
| `airplay-probe --selftest-mirror`（以发送端身份跑完整握手） | /info 200、pair-setup+pair-verify 200、fp-setup、SETUP 200、收到 1920×1080 关键帧，775ms 通过 |

未验证部分：本机没有可用的 iPhone / iPad / Mac，真实发送端的列表里能否出现，需要用户在手机上确认一次。若设备已出现但点选后失败，问题会落在握手阶段（已由上面的 selftest 覆盖到帧）或 Windows 防火墙的入站规则（检查过 `horizon-launcher.exe` 的放行规则为 Allow、Enabled）。

## 8. macOS 27 点选「镜像投屏」后握手失败

现场：iMacX（`192.168.0.122`，macOS 27.0，`AirPlay/980.77.5`）能发现 `HORIZON 86`，点选后镜像建立不起来。

### 8.1 线上抓到的真实序列

把 `HORIZON_AIRPLAY_TRACE=1` 打开后由 `airplay-probe` 直接对外广播，Mac 端一次点选抓到的请求序列（时间为秒，取自 trace7）：

| 时间 | 报文 | 说明 |
| --- | --- | --- |
| 0.004 | `POST /pair-verify` | Mac 缓存了上一轮的配对，先尝试恢复 |
| 0.132 | `GET /info` | 取能力表 |
| 0.154 | `POST /pair-setup` | 恢复失败，重新配对 |
| 0.219 / 0.341 | `POST /pair-verify` ×2 | M1/M3 |
| 0.430 / 0.442 | `POST /fp-setup` ×2 | FairPlay SAP，16→142、164→32 字节 |
| 0.458 | `SETUP rtsp://192.168.0.52/<session-id>` | 693 字节 bplist，**没有 `streams`** |
| 0.463 | 我们回 200（1414 字节） | 带 `timingPort` / `eventPort` / `info` |
| 0.489 | Mac 连上 `eventPort` | 事件通道真实建立 |
| 0.491 | `RECORD` | 文档里称 stage 2 |
| 0.512 | `TEARDOWN`（body 为 `{}`） | 距 RECORD 200 只有 **21 ms** |

关键事实：Mac **从未发送 `streams`（type 110）的第二条 SETUP**，也从未尝试任何镜像数据连接。也就是说它不是握手到一半超时，而是在 RECORD 之后 20 ms 内主动放弃。

SETUP 请求体（原样）：

```
combinedGetInfoWithControlSetup = true
deviceID = 46:A1:EC:A5:36:41        macAddress = 92:27:29:33:2F:EC
ekey（72 字节 FPLY…）               eiv（16 字节）     et = 32
isScreenMirroringSession = true     timingProtocol = "NTP"
timingPort = -13497（有符号读出，实际 UDP 52039）
model = MacBookAir10,1              name = iMacX      osVersion = 27.0
sessionUUID / sessionCorrelationUUID / updateSessionRequest = false
```

### 8.2 逐一定位的根因

1. **Windows 传出的 socket 继承了监听端的非阻塞标志**。监听端为了能响应「停止」而设成非阻塞，`accept()` 出来的连接沿用该标志，于是 Mac 连上后第一条请求撞上 `WouldBlock`，被我们当成 400 回掉。修法：`accept` 之后显式 `set_nonblocking(false)`，首包给 15 s 截止时间。
2. **镜像会话的 `RECORD` 回了 451**。451 是 RAOP「没有 SDP」的答案，镜像路径没有 SDP，Mac 收到 451 立刻中止。修法：只要会话声明了镜像（`ekey` 或 `isScreenMirroringSession`），RECORD 一律 200。
3. **SETUP 应答里的 `timingPort` / `eventPort` 是假的**（原来填的是 RTSP 端口甚至 0）。真实接收端给的是自己 bind 的 UDP NTP 端口和一条 TCP 事件通道；填 0 时发送端在 RECORD 后直接放弃。修法：`ensure_timing()` bind UDP 并应答 NTP，`ensure_event()` bind TCP 并保持连接。
4. **`combinedGetInfoWithControlSetup: true` 时必须在应答里内联 `info`**。否则发送端要再发一次 `GET /info`；现在它不再发，说明内联被接受了（trace7 与修复前的 trace6 对比可见）。
5. **`info.displays[0]` 的画布字段不全**。原来只有 `widthPixels` / `heightPixels`，缺 `width` / `height` / `uuid` / `receiverSupports444`，`rotation` 还写成 `true`。发送端按这条记录创建屏幕编码器，缺字段会在「准备 streams」阶段失败——这与 RECORD 后 20 ms 就 TEARDOWN 的时间吻合。已补齐 `width/height`（1920×1080）、稳定的 `uuid`、`receiverSupports444 = true`，`rotation` 改回 `false`。
6. **`pk` 写成了十六进制字符串**。真实接收端放的是 32 字节二进制公钥，发送端会把它拷进 endpoint 描述并用于配对校验。已改成 `Plist::Data`。
7. **`RECORD` 应答头不完整**：现在回 `Audio-Latency: 11025` 和 `Audio-Jack-Status: connected; type=analog`（与 RPiPlay/doubletake 一致）。原来的 `Audio-Latency: 0` 表示「没有音频时钟」。
8. **镜像音频流（type 96）的端口要从会话级 SETUP 应答里给出**。真实抓包里，会话 SETUP 的应答是 `{timingPort, eventPort, streams:[{type:96, controlPort, dataPort}]}`，Mac 随后只会再问 type 110。此前我们把 96 留到「收到了才分配」，Mac 拿不到音频端口就不会往下走。现在在会话 SETUP 应答里预分配并返回。
9. **NTP 的方向搞反了**。AirPlay 1 镜像里 **发送端才是时钟主机**：它在 SETUP 里给出自己的 `timingPort`，接收端要去探测它（RPiPlay 的 `raop_ntp_init(remote, timing_rport)`、doubletake 的 `ProbeLegacyTiming` 都是这个方向）。我们之前只在自己这侧起 NTP 服务端、等 Mac 来问，抓包里一次 NTP 请求都没收到，正好印证方向错了。现在除了保留服务端，还会主动向 Mac 的 `timingPort` 发 3 个探测并记录回包。

### 8.3 排除项

| 假设 | 证据 |
| --- | --- |
| mDNS / TXT 有问题，Mac 其实没解析到我们 | 抓包确认 `_airplay` / `_raop` PTR、SRV、A 全部正确；Mac 确实连上 5000 并走完配对 |
| 老式固定端口 7000/7001/7100 没开 | 同时监听这些 TCP/UDP 端口的探针在整个点选过程中没有收到任何连接 |
| 超时导致 TEARDOWN | 时间戳显示 21 ms 就断开，远小于任何超时 |
| 走的是 AirPlay 2 / TLS 路径 | 全程明文 RTSP + legacy pair-setup + FairPlay SAP，没有 TLS |
| `combinedGetInfoWithControlSetup` 处理错 | 修复前 Mac 会补一次 `GET /info`，修复后不再补，行为符合预期 |

### 8.4 本次验证

| 检查项 | 结果 |
| --- | --- |
| `cargo test -p horizon-cast --lib` | 181 passed / 0 failed / 4 ignored |
| 新增断言：`pk` 为 32 字节二进制、`displays[0]` 含 `width/height/uuid` | 通过 |
| 新增断言：镜像 RECORD 回 `Audio-Latency: 11025` + `Audio-Jack-Status` | 通过 |
| `airplay-probe --selftest-mirror` | /info 200（pk 二进制、display 1920×1080）、pair 握手、fp-setup、会话 SETUP 200、NTP 回环、110 SETUP 200、RECORD 200 (`11025`) 全通过 |

### 8.5 最终根因：`/info` 里的能力表是编的

按 8.2 修完 1–9 之后，Mac 仍然在 RECORD 后立刻 TEARDOWN（trace8–trace10，5–135 ms）。逐项对照 UxPlay（macOS 能正常投过去）的 `/info` 应答才发现，问题不在 SETUP / RECORD / 事件通道，而在**能力表本身**：

| 字段 | 修复前 | UxPlay | 影响 |
| --- | --- | --- | --- |
| `supportedFormats` | 手工填了 `screenStream: 0x1440800` | **不发布** | 发送端按这个掩码选镜像音频编码（ALAC / AAC-ELD）。声明了一个我们起不来的组合，发送端在 RECORD 后就直接放弃，连 110 流都不问 |
| `statusFlags` | `4` | `68` | 与 AirPlay 1 接收端的真实状态不符 |
| `Server` 响应头 | 缺失 | `AirTunes/220.68` | 发送端据此判断对端实现 |

修法：删掉 `supportedFormats`（AppleTV3,2 本来就没有），`statusFlags` 改 68，所有 RTSP 应答补 `Server: AirTunes/220.68`；会话 SETUP 的应答收敛成 `{timingPort, eventPort}`（去掉 `skipRecord`），`ekey/eiv/isScreenMirroringSession` 的解析提到 `streams` 分支之前，兼容 media-first 的发送端。

### 8.6 修复后的线上验证（trace11）

同一天用同一台 Mac（macOS 27.0）与 iPhone（iOS 15.8.8）各点一次「镜像投屏」，探针日志：

| 时间 | 事件 | 结果 |
| --- | --- | --- |
| 0.789 | Mac `SETUP` 带 `streams:[{type:110, streamConnectionID:-4717205613216837993, latencyMs:100, timestampInfo:[SubSu,BePxT,AfPxT,BefEn,EmEnc]}]` | 200，85 字节应答 |
| 0.79x | `mirror stream connected by 192.168.0.122:63188` | 数据连接建立 |
| 1.0x | `mirror codec config 1920x1080` | SPS/PPS 解析成功 |
| 后续 | 镜像帧持续到达，Mac 端会话持续 41 s 后由用户主动结束 | 成功 |
| iPhone | `streamConnectionID 3788043780171579770`、`mirror codec config 608x1080`，`POST /feedback` 每 2 s 心跳 | 成功，会话保持 |

也就是说：**握手、110 流、7100 数据通道、H.264 码流在真实发送端上全部打通**。

### 8.7 「接收端不显示投屏界面」的第一层原因

`HORIZON 86` 这个名字当时由 `airplay-probe` 广播，而它是一个**无界面**的协议探针（专用于抓包验证），收到画面只统计不解码显示。真正的接收界面在 `HORIZON Meeting` 应用里：投屏面板 → AirPlay 接收开关 → `CastPanel` 的 `MirrorPreview`（WebCodecs `VideoDecoder`）与 `CastStage` 全屏舞台。广播名取自设置的「设备名称」（默认就是 `HORIZON 86`），所以应用与探针在发送端列表里是同名的——必须停掉探针，否则分不清投到了哪一侧。

### 8.8 应用侧复测：连上了、帧到了，画面仍是黑的

用修复后的 `horizon-cast` 重新出包并启动应用，发送端（`192.168.0.12`，镜像分辨率 1920×1080）点选后：

| 观察点 | 结果 |
| --- | --- |
| `TCP 192.168.0.24:7100 ← 192.168.0.12:59057` | ESTABLISHED，镜像数据通道建立（本机有多个地址，面板显示 `192.168.0.52:5000`，发送端实际连到 `.24`） |
| 面板「当前状态」 | 正在镜像 |
| 面板「镜像画面」 | 1920×1080 · 已配对，且出现「镜像帧 N」统计行 → 帧在持续解密送达 |
| 全屏舞台 / 卡片预览 | 黑屏，提示停留在「等待 iPhone、iPad 或 Mac 开始镜像」 |

预览的 `phase` 停在 `waiting`（不是 `error`/`resyncing`），说明它**一个关键帧都没收到**：解码器只在收到 IDR 时才 `configure`。原因是时序，不是解码：

1. 发送端建立 7100 时只发**一条开场 IDR**，之后全是增量帧；
2. 应用侧靠 1 s 一次的 `receiver_status` 轮询才发现 `mirroring`，此时卡片预览因 `active={!presenting}` 卸载、舞台预览才挂载；
3. 新挂载的解码器错过开场 IDR，`FrameHub` 在没有通道时直接丢帧，于是永远等不到可解码的入口。

修法（`src-tauri/src/receivers.rs` 的 `FrameHub`）：引擎线程送来的每一帧都记进一段**从最近 IDR 起、到最新帧止的连续缓冲**（上限 120 帧 / 16 MB，超限即整段作废，因为掐头就解不开）；预览挂载时把这段整份交给它回放，回放队列优先于实时队列，且回放期间实时队列不受 4 帧积压上限约束（否则会在「回放尾巴」和「直播头部」之间留下空洞，而接收端没办法让发送端再补一个 IDR）。这样无论是自动进入全屏舞台、还是中途开关面板，晚到的解码器都能从 IDR 起步并立刻追上直播。

### 8.9 真正的根因：镜像帧的关键帧判定读错了字节

8.8 出包后用户完成点击，面板读数是关键的一条证据：

> 镜像帧 **223**，关键帧 **0**

帧在到，解码器却一个入口都没拿到——但原因不在时序，`FrameHub` 的回放缓冲根本无从填充：引擎把每一帧都判成了增量帧。

`crates/horizon-cast/src/airplay/mirror.rs` 里只看了 AVCC 首片的第 5 个字节：

```rust
let keyframe = payload.get(4).is_some_and(|byte| byte & 0x1f == 5);
```

这只在「一个访问单元以 IDR 开头」时成立。macOS/iOS 的镜像发送端把访问单元写成 **AVC 定界符（NAL type 9）在前、IDR（type 5）在后**，第 5 个字节是 `0x09 & 0x1f = 9`，于是 223 帧全部记成非关键帧，`stats.keyframes` 恒为 0，`MirrorPreview` 的 `phase` 永远停在 `waiting`（不是 `error`——它压根没收到可配置解码器的帧）。浏览器扩展那条链路之所以正常，是因为扩展发的正是以 IDR 开头的访问单元。

修法：AVCC→Annex B 转换之后扫描整段访问单元的 NAL 头，用「是否包含 type 5」判定关键帧（`nal_types()`，靠模拟抑制字节保证扫描不会凭空造出起始码）；同时给首帧打一条 trace，记录长度与 NAL 类型列表，避免下次再靠猜。

回归测试同步改了：`crates/horizon-cast/src/airplay/mirror.rs` 的 `a_scripted_sender_streams_one_key_frame` 现在按真实发送端的形式构造 AVCC（`[0x09]` 定界符 + `[0x65,1,2,3,4]` IDR），期望帧数据为
`00000001 67420000 00000168 ce` + `00000001 09` + `00000001 6501020304`（SPS/PPS 前缀 + 定界符 + IDR），即定界符必须原样保留在 Annex B 流里，关键帧标志为真。`cargo test -p horizon-cast` 全绿：**181 passed / 0 failed / 4 ignored**。

8.8 的回放缓冲并非无用——它解决的是「晚到的解码器」，本节解决的是「根本没有 IDR 被认出来」；两者叠加，画面才可能既进得来又追得上。

### 8.10 修复后的实测：画面已渲染

18:07 用 `npx tauri build` 出包（`target/release/horizon-launcher.exe` + NSIS 安装包），启动后由 Mac（`192.168.0.12`）重新点选镜像，用 Windows 图形捕获接口直接截取应用窗口验证：

| 观察点 | 结果 |
| --- | --- |
| `TCP 192.168.0.24:7100 ← 192.168.0.12:61010 / 61033` | ESTABLISHED，镜像数据通道建立 |
| 面板「镜像帧 / 关键帧」 | **1273 / 1**（修复前是 223 / 0）→ 关键帧被正确识别 |
| 全屏舞台 `CastStage` | 顶部「正在接收 · AirPlay 镜像 1920×1080 · 已配对」，画布上出现 Mac 桌面的真实内容（Safari 打开 `cn.orionstar.com/greeting-robot2.html`；第二次会话则是 macOS 的「从 ⧉ 菜单中选择镜像或扩展显示器」提示页），文字边缘清晰 → WebCodecs 解码 + `drawImage` 上屏正常 |
| 会话断开后重连 | 舞台自动重新弹出并继续出画，无需重启应用 |

一个值得记住的量化事实：**整场会话只有一个关键帧**（发送端在 7100 建流时发一条开场 IDR，之后 1200+ 帧全是增量帧，没有周期性 IDR）。所以 8.8 的回放缓冲不是可选优化，而是「晚挂载的解码器能看到画面」的唯一出路；也意味着接收端在缓冲上限（120 帧 / 16 MB）之外接入时，只能等发送端自己再出一个 IDR。

### 8.11 延迟与卡顿：从根因逐项打透

用户反馈：画面已能显示，但「延迟太严重，太卡」。先把链路拆成可测的四段，再逐段量化，而不是直接猜。

**仪表先行**。接收端只有一处计时是可信的：发送端 PTS 与本机单调时钟的相对关系。绝对 NTP 差值受两端时钟偏移污染（上一轮实测显示 0 ms 即为此因），因此新增「相对漂移」——以会话首帧的 `(pts, 本机时刻)` 为基准，后续帧的 `实际经过时间 − 发送端时间轴经过时间`，偏移量在相减中抵消，只剩排队。同批加入的还有到达间隔（均值/峰值，卡顿的直接读数）、`BridgeSink` 每帧移交耗时，以及 webview 侧经 `mirror_perf_report` 回报的上屏帧率、到达到绘制、解码队列峰值、追帧丢弃数（`src-tauri/src/receivers.rs` 的 `MirrorPerf`）。窗口化 trace（每 100 帧一条，`HORIZON_AIRPLAY_TRACE_FILE`）让「引擎侧到达」与「预览侧上屏」落在同一份日志里对时。

**基线（18:07 构建，未含本轮优化）**：最大帧间隔 2830 ms，`BridgeSink` 每帧 0.01 ms——移交不是瓶颈，瓶颈在移交之前或之后。单独测 AES-CTR：33 MB/s，即一帧 200 KB 要 6.06 ms，全在 socket 线程上；CNG 的 `BCryptEncrypt` 每 16 字节一次 FFI 调用是根因。

| # | 根因 | 证据 | 修复 |
| --- | --- | --- | --- |
| 1 | 解密每 16 字节一次 FFI 调用 | 33 MB/s，200 KB 帧 6.06 ms | `crypto.rs` 的 `AesCtr` 改为一次生成 32 KB 密钥流（2048 个连续计数器一次 ECB），**2126.8 MB/s，0.094 ms/帧**，快 64 倍；NIST 向量、跨调用残余块、pair-verify 双签名续流三条正确性测试原样通过 |
| 2 | 每帧两遍扫描 | `avcc_to_annex_b` 转换后再 `nal_types()` 全帧扫起始码 | 关键帧判定合进 AVCC 转换那一遍（NAL 头就是被覆写长度前缀的下一个字节），`nal_types()` 只留给首帧 trace；新增「IDR 不在首位仍判定为关键帧」回归用例 |
| 3 | 预览画布按源分辨率铺满 | `canvas.width = frame.displayWidth`，卡片里也只有几百 CSS 像素，却每帧复制 1920×1080 像素 | 用 `ResizeObserver` 量实际盒子，背衬尺寸取 `min(源尺寸, 盒子×dpr)` 且保持源宽高比，按 vsync 缩放 |
| 4 | 解码器突发积压时逐帧重绘 | WebCodecs 输出回调里直接 `drawImage`，落后至多秒级仍全部上屏 | 只上屏「不超过最新到达帧 100 ms」的帧（`PAINT_LAG_US`），旧帧立即 `close()` 计数为追帧丢弃；**解码不能跳**——AirPlay 镜像整场只有一个开场 IDR，跳过任何一个增量帧都会让参考链断掉且无法恢复 |
| 5 | 一次 vsync 内多次绘制 | 积压排空时同一显示帧内多张图 | `requestAnimationFrame` 合并，每显示帧最多一次 `drawImage`，`pending` 只保留最新一张 |
| 6 | 转发队列过浅造成参考链空洞 | `FRAME_QUEUE = 4`，溢出即丢最旧帧并标记空洞；对无周期 IDR 的镜像流，空洞之后前端只能等永不到来的关键帧 | 队列提到 16（约 0.5 s）。延迟由预览侧「只画最新」来兜，而不是靠丢参考帧——后者换来的是黑屏 |

### 8.12 实测数据（19:36 构建，含根因 1/2/3/6，未含追帧修正）

Mac（`192.168.0.12`）镜像到本机，`HORIZON_AIRPLAY_TRACE_FILE` 同时记录引擎侧与预览侧的一秒窗口（`preview window` 由 `mirror_perf_report` 写入同一文件）：

```text
mirror window: 100 frames 1079 KB, gap avg 33.2 ms max 79.6 ms, behind max 31.6 ms, hand-off avg 0.02 ms
preview window: paint 12 fps, skip 18, queue max 1, arrival to paint avg 133.1 ms max 170.0 ms
```

| 指标 | 基线（18:07） | 本轮（19:36） | 判定 |
| --- | --- | --- | --- |
| 引擎侧到达间隔 | 最长 2830 ms | 平均 33.2 ms（=30 fps）、最长 ≤87 ms | 到达节奏已经稳 |
| 落后实时（漂移） | 未测 | 平均 5–29 ms、窗口峰值 ≤32 ms，多个窗口为 0.0 ms | 全链路不再累积排队 |
| `BridgeSink` 每帧 | 0.01 ms | 0.01–0.02 ms | 移交从来不是瓶颈 |
| 解码队列峰值 | 未测 | 1–2 帧 | 解码供得上 |
| 预览上屏 | 未测 | **12–14 帧/秒**（到达 30 帧/秒） | 追帧阈值写错，见下 |
| 到达到绘制 | 未测 | 平均 133 ms、峰值 ~208 ms | 平台解码器流水线，见下 |
| 转发丢帧 | 未测 | 0 | 队列 16 帧没有溢出过 |

两条新发现的根因（同轮修正）：

- **根因 7：追帧阈值拿错了参照物。**「比最新到达帧旧 100 ms 就不画」看似合理，但硬件解码器本来就要攒 3–4 帧才出第一帧——`queue max 1` 而「到达到绘制 133 ms」正是这个固定流水线，于是每一张输出图在诞生时都已「过期」，30 帧里只有 12 帧被画出来。正确参照是「更新的**已解码**帧」：改为 `requestAnimationFrame` 合并，一个显示帧只画手头最新的一张，被它顶掉的才计入追帧丢弃。
- **根因 8：平台解码器的流水线本身就是延迟。**1080p 软解只占一个核的一小部分，换来单帧输出。镜像路径改为 `prefer-software` 起步（浏览器扩展路径同步），解码报错时再回退平台解码器；原先「出错才切软解」的方向保留，只是起点换了。

另一个被证实的事实：镜像视频帧的 PTS **不是墙钟**。首帧 trace 显示 `mirror first pts 238327267166 (local NTP 4002262728355712, age 4002024401088 ms)`，两场会话的首帧 PTS 相差 33.1 s，而本机墙钟只差 0.3 s——发送端用的是自己的单调时钟。所以面板上「绝对延迟」一行对 AirPlay 镜像永远无意义（60 s 合理窗把它判成 0 ms），要么按会话基准换算，要么删掉；本轮按「按会话基准」的相对漂移为准。同时首帧 NAL 列表 `[6, 5]`（SEI 在前、IDR 在后）再次印证 8.9 的判定：只读第 4 个字节必然读错关键帧。

尚未解决的下一档：`/info` 的 `maxFPS` 仍是 30，发送端据此把镜像压在 30 帧/秒（到达间隔 33.2 ms 即其直接证据）。业内第一梯队的 AirPlay 2 接收端做 60 帧/秒，所以这一步要单独改、单独测。

### 8.13 根因 9：投屏抽屉自己的毛玻璃把画面压到 27 帧/秒

20:02 的构建（含根因 7/8）实测，Mac 镜像、桌面有内容变化时：

```text
mirror window: 100 frames 650 KB, gap avg 33.3 ms max 67.8 ms, behind max 0.0 ms, hand-off avg 0.02 ms
preview window: paint 18 fps, skip 12, queue max 0, arrival to paint avg 19.9 ms max 45.0 ms
```

| 指标 | 19:36 | 20:02 | 判定 |
| --- | --- | --- | --- |
| 到达间隔 | 33.2 ms | 33.3 ms（=30 帧/秒），最长 67.8 ms | 引擎侧到位 |
| 落后实时 | ≤32 ms | **0.0 ms**（个别窗口 153 ms 后追回） | 不再排队 |
| 到达到绘制 | 133 ms | **平均 20 ms、峰值 45 ms** | 根因 8 的硬件流水线消失 |
| 解码队列峰值 | 1–2 | 0–4 | 软解供得上 |
| 上屏 | 12–14 帧/秒 | 17–20 帧/秒，追帧丢弃 11–13 帧/秒 | 仍未到 30，见下 |

上屏 18 帧/秒而到达 30 帧/秒，且丢弃的正好是「同一显示帧里积压的第二张」——说明天花板不在解码，而在**显示帧本身的节拍**。于是用 WebView2 的 CDP（9333）在页面内直接量 `requestAnimationFrame`：

| 测法 | rAF 节拍 | 旁证 |
| --- | --- | --- |
| 镜像关闭、页面静止 | 60.4 帧/秒 | 合成器本身健康 |
| 镜像流动、逐帧搬运 | **20.4 帧/秒** | 窗口已聚焦（`document.hasFocus()` 为 true，`Page.bringToFront` 后仍 21.3）→ 不是后台降帧 |
| 同场景，`* { backdrop-filter: none }` | 60.0 帧/秒 | 一次注入即恢复 → 不是 CPU、不是 GPU 型号不行 |
| 主线程探针（4 s 窗口） | 宏任务 43 216 次/秒，长任务 0 个 | 主线程完全空闲，瓶颈在合成/呈现侧 |

本机 GPU 是 `ANGLE (AMD, AMD Radeon R5 220 … Direct3D11)`，窗口 1872×1080。为了不等 Mac 也能复现，改成在**真实那块预览画布**（backing 960×540、CSS 426×239）上用 rAF 循环 `drawImage` 一张 1920×1080 的源图，量同样的搬运+呈现成本：

| 每帧做什么 | rAF 帧/秒 | JS 侧 `drawImage` 调用均耗 |
| --- | --- | --- |
| 空循环 | 57.8 | 0 ms |
| 1080p → 预览画布（开平滑） | 27.8 | 0.04 ms |
| 1080p → 预览画布（关平滑） | 26.8 | 0.05 ms |
| 1080p → 1920×1080 backing | 23.9 | 0.05 ms |
| 同上四种，但全页禁用 `backdrop-filter` | 全部 60.0 | 0.04–0.05 ms |

`drawImage` 的 JS 调用是异步派发的（0.04 ms 说明不了什么），真正代价发生在合成阶段：投屏面板本身就是抽屉，`.overlay--drawer` 带着 `backdrop-filter: blur(9px)` 铺满 0,0,1872,1080（`src/components/Overlay.tsx:32` 渲染 `overlay overlay--${variant}`，`src/styles/overlays.css:47` 定义模糊）。视频层每重绘一次，这块全屏模糊面就要重算一次——在 R5 220 上就是 3 帧/秒只出 20 帧。命中列表里压在预览画布矩形（1382,436,426,239）上的还有 `.cast-preview__open`（`blur(8px)`，1723,443,76,26）与 `.sidebar--right`（`blur(12px)`，1790,340,62,400）；`.dock` 的 `blur(16px)` 在 369,843 不重叠。关闭平滑没有差别，说明代价来自模糊重算而不是缩放滤波。

修法按状态收窄，不动常规页面观感：`MirrorPreview` 在有画面可画期间给 `<body>` 挂 `is-streaming`（用模块级计数 `livePreviews` 兜住卡片与全屏舞台交接的那一帧，`src/components/MirrorPreview.tsx`），样式里 `body.is-streaming` 下让 `.overlay`、`.overlay--drawer`、`.sidebar`、`.dock`、`.cast-preview__open` 的 `backdrop-filter` 退回 `none`（`src/styles/overlays.css`「Live stream」一节）。会话结束类移除，毛玻璃照旧。

同轮把 `/info` 的 `maxFPS` 从 30 提到 60（`crates/horizon-cast/src/airplay/session.rs:1239`，与既有的 `refreshRate = 1/60` 对齐），发送端是否真发 60 帧由到达间隔验证：仍是 33 ms 就说明它另加了闸门（`features3` 的 0x80000/0x100000 两个 60 帧标志位本轮**没有**盲改，避免把已经打通的握手再改坏）。

### 8.14 修复后实测（20:2x 构建，含根因 7/8/9 与 `maxFPS 60`）

同一台 Mac（`192.168.0.12`）镜像、同一台接收机（AMD Radeon R5 220、1872×1080 窗口）。`live-check` 一次采样同时给出合成状态与面板读数：

```json
{ "streaming_class": true, "raf_fps": 60.3, "worst_raf_gap_ms": 22.2,
  "blur": { ".overlay--drawer": "none", ".sidebar--right": "none", ".dock": "none", ".cast-preview__open": "none" } }
```

```text
mirror window: 100 frames 68 KB, gap avg 21.8 ms max 60.1 ms, behind max 0.0 ms, hand-off avg 0.01 ms
preview window: paint 36 fps, skip 14, queue max 0, arrival to paint avg 11.0 ms max 26.2 ms
```

| 指标 | 18:07 基线 | 19:36 | 20:02 | 20:2x（本轮） |
| --- | --- | --- | --- | --- |
| 到达间隔（引擎侧） | 最长 2830 ms | 33.2 ms | 33.3 ms | **16.2–22.1 ms**（最快窗口 ≈60 帧/秒） |
| 落后实时 | 未测 | ≤32 ms | 0.0 ms | 0.0–23.4 ms，稳态窗口多为 0.0 |
| 到达到绘制 | 未测 | 133 ms | 20 ms | **9.5–11 ms（峰值 20–26 ms）** |
| 解码队列峰值 | 未测 | 1–2 | 0–4 | 0–2 |
| 上屏帧率 | 未渲染 | 12–14 | 17–20 | **26–36** |
| rAF 节拍（镜像流动中） | 未测 | ~20 | 20.4 | **57.1–60.3** |
| 转发丢帧 | 未测 | 0 | 0 | 32（仅接入瞬间，稳态 0） |

四条判定：

- **`maxFPS 60` 被发送端接受。**最快窗口到达间隔 16.2/17.0/17.1 ms（≈60 帧/秒），而 19:36 与 20:02 两轮所有窗口都钉在 33.2/33.3 ms——只改 `/info` 的显示属性、不动 `features3`，macOS 就把镜像从 30 帧/秒提到了 60 帧/秒。引擎侧同窗口 `behind max 0.0 ms`、`hand-off 0.01 ms`，说明 60 帧/秒下接收链路依旧零排队。
- **合成层修复在产品真实状态下生效。**`is-streaming` 挂上后四处 `backdrop-filter` 全部计算为 `none`，镜像流动时 rAF 从 20.4 回到 57.7–60.3。注意 `.overlay`/`.dock` 带 `transition-property: all`，类挂上后要等过渡走完（实测 ~1.5 s）计算值才落到 `none`；第一次采样读到了过渡起始值，误判成「规则没生效」，稳态复测才是结论。
- **本机剩下的天花板是 1080p 舞台的搬运，不是软件链路。**8.13 的合成实验里同一块画布，960×540 backing 能保 60 帧/秒，1920×1080 backing 只能到 41 帧/秒；全屏舞台正是 1872×1080，所以本机实测上屏 26–36 帧/秒而不是 60。换一块正常显卡这一条即消失，本轮不为它牺牲 1080p 舞台的清晰度（降 backing 换帧率会糊掉正文与小字）。
- **一处一次性风险记录在案。**`转发丢帧 32` 只出现在预览接入的那一秒：回放缓冲（最多 120 帧）洪泛叠加舞台挂载，`FRAME_QUEUE = 16` 溢出。溢出会打空洞标记，而 AirPlay 镜像整场只有一个开场 IDR（本轮 6890 帧 / 2 个关键帧，第二个来自重连），前端遇到空洞只能等永不到来的关键帧——理论后果是画面冻结。本轮未复现冻结（丢帧计数停在 32，上屏持续 26–36 帧/秒），故不改代码，只记为已知风险与后续项。

延迟预算（本轮，本机可测部分）：发送端在 SETUP 的 `timestampInfo` 里自报约 100 ms 编码流水线 + 本机引擎侧零排队 + 到达→绘制 10–11 ms + 最多一个显示帧（16.7 ms）≈ **端到端 130–140 ms**，其中接收端自身占比已不到 10%。

仍未覆盖：镜像音频（AAC-ELD）不解码，面板上已明写；个别窗口出现 60.1–4006 ms 的到达停顿（`behind max 716 ms` 后追回），发生在发送端或 Wi-Fi 侧，本机队列没有放大它。

### 8.15 本轮量测中撞出的新缺陷：退出全屏后面板没有画面

复现路径（20:2x 构建，Mac 正在镜像）：面板卡片出画 → 点「全屏观看」→ 舞台出画 → 点「退出全屏」。**卡片回到「等待 iPhone、iPad 或 Mac 开始镜像」，而引擎侧仍是「正在镜像」、`镜像帧 10176，关键帧 3`**（截图 [`shots/airplay-card-blank-after-stage.png`](shots/airplay-card-blank-after-stage.png)）。

原因是两个显示面各自挂一个解码器：`CastPanel` 的卡片是 `<MirrorPreview active={!presenting}>`（`src/components/CastPanel.tsx:776`、`:921`），舞台是另一个实例（`src/components/CastStage.tsx:20`）。全屏时卡片 `active` 转 false，管线整个拆掉；退出时重新 `attach()`，而 `attach()` 之前那次会话的开场 IDR 早已用掉——`join` 回放缓冲上限 120 帧（有画面变化时约 2–4 秒）就会整段清空（`src-tauri/src/receivers.rs:216`），于是新挂载的解码器只能等一个不会来的关键帧。这和 8.10「晚挂载必须靠回放」是同一枚硬币的另一面：回放救得了「会话开始后几秒内挂载」，救不了「会话中途换面」。

正确的修法只有一个方向：**一次会话一个解码器，两个面共用**。两个候选：

- 卡片与舞台共用同一个 `MirrorPreview` 实例，「全屏」只是把它从卡片盒子撑到 `inset: 0`（需要绕开抽屉的 `transform`/`backdrop-filter` 使 `position: fixed` 失效的问题，故要把它 portal 到 body 并按卡片矩形定位）。改动集中在呈现层，代价最小，且退出全屏是零等待。
- 转发中心支持多路 `Channel` 扇出，卡片与舞台同时持有各自的流。代价是两份解码与两次搬运——正好是 8.13 里被证明最贵的那一步。

本轮未动代码：这是与「延迟/卡顿」不同的另一条缺陷线，且两种改法都会改变呈现流程，需要单独一轮验证。

### 8.16 8.15 的修复：一次会话一个解码器，两个面共用同一块画布

选了第一条方向（共用一个解码器），但落地形态不是「把 `MirrorPreview` 搬进 portal」：React 换 portal 容器会让子树整体重挂载，等价于又装了第二个解码器。所以把所有权下沉一层——

- `useMirrorPreview` 成为一个源的解码器**和** `<canvas>` 的所有者（`src/components/MirrorPreview.tsx:61`）。面板里每个源各一个实例：AirPlay 在 `src/components/CastPanel.tsx:116`，浏览器扩展在 `:121`。解码器只在实例挂载时 `attach()` 一次，会话中途不再重建。
- `MirrorSurface`（`src/components/MirrorPreview.tsx:657`）退化成一个纯宿主盒子：卡片用它（`CastPanel.tsx:854`、`:1004`）、全屏舞台用它（`src/components/CastStage.tsx:43`）。谁成为宿主，画布就被**命令式搬过去**（`host.insertBefore(canvas, host.firstChild)`，`MirrorPreview.tsx:85-91`），DOM 节点不变 → WebCodecs 解码器与已喂进去的开场 IDR 都不变。
- 于是 8.15 的前提消失：退出全屏不再有「新挂载的解码器等一个不会来的关键帧」这一步，回放缓冲（`src-tauri/src/receivers.rs:216`）只服务它本来的场景（会话开始后几秒内挂载）。

实机验证（21:05 构建，Mac 正在镜像 1920×1080）：卡片出画 → 点「全屏观看」舞台出画 → 点「退出全屏」，卡片**继续显示 Mac 实时画面**，顶栏仍是「正在镜像 · 1920×1080 · 已配对」，面板读数 `镜像帧 3583，关键帧 1`、上屏 40 帧/秒、到达到绘制 9 ms（峰值 19 ms）（截图 [`shots/airplay-card-after-stage-fixed.png`](shots/airplay-card-after-stage-fixed.png)）。**整场会话只有 1 个关键帧而卡片换了面依然有画**，正是这条修复要证明的那件事。

同一改动在浏览器扩展源上给出可重复（不需要 Mac）的断言，最终构建（22:39）经 CDP 读回：进入全屏 `sameCanvas: true`、退出后 `canvasBackInPanel: true` 且 `sameCanvasAfter: true`——两块面之间往返的是同一个 canvas 节点。

## 9. 22:0x–22:4x 轮：投屏码、Miracast 入口、大屏信息、扩展自动发现

### 9.1 投屏码 = 6 位 token，并且真的印在屏上（#75 / #77）

- 码就是浏览器扩展通道的 token，`{:06}` 生成（`src-tauri/src/receivers.rs:519`）。它落在 `app_config_dir/browser-token`（`:537`）：只有「长度 6 且全是数字」才复用，所以**重启应用不换码**——扩展里存过的地址下次还能直连。本轮最终构建（22:39 重启后）读回仍是同一个 `518294`。（23:5x 补记：这条有前提——同一份 `browser-token` 文件被两版应用共用，见第 10 节末尾。）
- 接收端开启后三处可见：卡片事实行「投屏码」（`CastPanel.tsx:957`）、预览窗居中字卡（`:1013`，样式 `src/styles/overlays.css:1404`）、全屏舞台把码当主角（`overlays.css:1597`）。舞台字号的实测计算值：`font-size 152px`、`letter-spacing 21.28px`、显示字体——三米外能读。
- 撞出的视觉缺陷：预览盒自带的「等待…」提示会**从数字缝里透出来**。修法是让「等待」这件事只由一处说：卡片加 `.is-waiting`（`CastPanel.tsx:1007` → `overlays.css:1444`），舞台加 `.cast-stage--waiting`（`CastStage.tsx` → `overlays.css:1587`），两处各自 `display:none` 掉那条提示。修前修后各一张图（`shots/card-code-22-30.png`、`shots/stage-code-final.png`）。
- 连上以后字卡让位给画面（`status.connected` 时不渲染浮层），事实行保留码，方便现场核对「投的是这台」。

### 9.2 发现端点 `/info` 与私有网络预检（#75）

扩展不能组播，所以发现靠 HTTP 扫段；扫描要能读、又不能泄码：

- `info_reply`（`crates/horizon-cast/src/browser.rs:500`）只回 `{service, name, port, path, paired}`，带 `Access-Control-Allow-Origin: *`、`Access-Control-Allow-Private-Network: true`、`Cache-Control: no-store`。**里面没有任何 token 字段**——`paired` 只是布尔，告诉发送端要不要输码。
- `http_reply`（`:522`）先认 `OPTIONS` 并回 204 预检（`:493`）。这不是可选项：现在的 Chrome 把扩展页发起的局域网请求算作本机网络访问，预检没人答就永远读不到 `/info`，接收端在扩展里等于隐身。
- 线上实测（最终构建，本机 `192.168.0.24:9407`）：`GET /info` → 200，头里有 `Access-Control-Allow-Origin: *` 与 `Access-Control-Allow-Private-Network: true`，正文只有 `{"name":"戎耀会","paired":true,"path":"/stream","port":9407,"service":"horizon-browser"}`——**没有码**；`OPTIONS` → 204 + `Allow-Methods: GET, OPTIONS` + `Allow-Private-Network: true` + `Max-Age: 600`；WebSocket 无 token 96 ms 被关、错码 37 ms 被关、正确码 53 ms 完成握手。
- 回归网：`cargo test -p horizon-cast browser` 12 条通过，其中两条正是「`/info` 不泄漏投屏码」和「私有网络预检被回答」。

### 9.3 Cast 接收端下线，换 Miracast 系统入口（#74）

- 投屏面板里 Cast 接收端卡片已移除（探针断言 `castReceiverGone: true`），换成的 `MiracastCard`（`src/components/CastPanel.tsx:639`）只做入口，不重造接收端。引擎文件 `crates/horizon-cast/src/cast_recv.rs` **保留不删**：本仓库不是 git 仓库，删除不可回退，而 `browser.rs` 仍在用它的 `sha1`。
- 判定与动作：`miracast_launcher()`（`src-tauri/src/lib.rs:333`）探 `System32\cast.exe` / `connect.exe`；装了就 `miracast_launch` 直接起系统全屏接收（toast「已启动系统无线投屏，请在发送端选择本机」），没装就打开设置页。
- 本轮撞出并修掉的缺陷：两个按钮原先都指向 `ms-settings:project`（快速投屏 flyout）——未装组件的那台机器上，那里没有可装的东西。现在指向 **`ms-settings:projections`**（`lib.rs:287`、`:357`，`src/lib/bridge.ts:77` 同步）。页面内容用 UIA 实读为证：面包屑「系统 / 投影到此电脑」，正文 `添加 "无线显示器" 可选功能以投影到此电脑:` 加一个「可选功能」链接。发布出去的二进制里 `ms-settings:projections` 出现 1 次、终止式的 `ms-settings:project` 出现 0 次。
- 顺带收掉重复：未安装时主按钮就叫「打开「投影到此电脑」设置」，第二个跳同一页的按钮不再渲染（`CastPanel.tsx:683`）。本机现状 badge「需要安装组件」，最终构建实测卡片动作区只剩这一个按钮（`buttons: ["打开「投影到此电脑」设置"]`），提示文案与真实落点一致。
- 链路的组合方式说清楚：按钮 → `open_builtin` / `miracast_launch` 会真的拉起系统设置，这一环上一轮用「先杀进程再看它重新出现」证过；URI → 页面内容这一环本轮手工导航同一字符串并用 UIA 读回页面文本证过；编译产物里只有新 URI（上一条）。本轮**没有**再点一次产品按钮，因为验证时用户正开着系统的「添加设备」对话框装「无线显示器」组件，抢它的前台窗口会打断对方。

### 9.4 扩展：自动发现接收端 + 输码即投 + 圆角（#76）

- 发现：`candidateHosts()`（`extension/popup.js:131`）用 `chrome.system.network.getNetworkInterfaces` 取本机 IPv4 前缀，对 /24 逐地址打 `http://<host>:9407/info`。48 个并发 worker、单主机 700 ms 超时、结果每 120 ms 合并重绘一次——**边扫边画**，操作者不用等最后那批哑地址。508 个地址：24.4 s → 7.8 s（最终构建复测 7 795 ms 扫完）。
- 选中：按「上次连过的 > 局域网地址 > 回环」排序（`hostRank`，`:122`）。这条是量测里撞出来的：原先谁先答谁当选，回环常常抢赢，扩展默认就连到 `127.0.0.1`。本轮扫到 2 个应答（WLAN `192.168.0.24` 与虚拟网卡 `172.20.80.1`，同一台机器的两块面），选中的是 WLAN。
- 输码闸门：不满 6 位不放行（提示「请输入大屏上显示的 6 位投屏码」）、非数字被吃掉（`1234a9b9` → `123499`），拼出的地址是 `ws://192.168.0.24:9407/stream?token=518294`；`{code, receiverHost}` 存进 `chrome.storage.local`，下次打开面板即选中。手填地址的兜底路径同样带 token（`ws://10.1.2.3:9407/stream?token=246810`）。
- 外观：computed 圆角实测 hero 20px、主操作 13px、接收端行 19px、投屏码输入框 14px（截图 `shots/ext-stub-idle.png`、`shots/ext-stub-live.png`）。
- **诚实边界**：这台机器上 Chrome 137+/Edge 一律忽略 `--load-extension`（headless、headed、加 `--enable-unsafe-extension-debugging` 都试过，扩展 ID 也算出来访问过，popup 始终不出现）。所以扩展侧的验证是把**未改动的 `popup.html` / `popup.js`** 跑在真浏览器里，只桩掉 `chrome.storage / runtime / tabs / tabCapture / system.network` 五个 API，页面来源用局域网地址 `http://192.168.0.24:8123` 而不是回环，绕开安全上下文的本机网络闸门。也就是说：发现、排序、输码、拼 URL、持久化这几步跑的是 shipped 代码。
- 这条链路的边界要看清：桩里跑出的「正在投放 · 1920×1080 · 32 fps · 8.4 Mb/s」是 popup 自己的呈现，同一时刻接收端卡片仍是「等待浏览器扩展连接」——所以它证明的是**决策链**，不是传输。传输那一半另有两条独立证据：9.2 的真实 WebSocket 握手（正确码 53 ms `open`、无码与错码被关），以及任务 #40 用真实 Chrome/Edge 扩展跑通的整轮投放。抓取与编码链路本轮未改动。

### 9.5 大屏信息可整体隐藏（#73）

- 设置里一个开关（`src/components/Settings.tsx:154`），关掉后 `src/components/Hero.tsx:26` 直接不渲染大屏名称、欢迎语与日期：主屏文本从「MEETING READY / 会前就绪 / 戎耀会 / 让每一次协作，都清晰发生 / 10月29日星期四 / 6 个快捷入口 / 本地模式」变成空串，容器子节点数 4 → 0。开关自己的说明同步改口（「名称与欢迎语已隐藏，随时可在这里打开。」），再打开恢复原文案与 4 个子节点。
- 关键约束是「隐藏信息，不隐藏版式」：`.hero` 仍占 top 51 / 高 707，dock 仍停在 top 801 / 高 167，图标不跳位——品牌壁纸自带设计好的信息时，这块地方让给壁纸，其余布局照旧。
- 状态存在 `launcher-state.json`，纯前端设置项，不需要 Rust 侧改动。

### 9.6 本轮仍未覆盖

- AirPlay 镜像音频（AAC-ELD）不解码，面板已明写。
- 这台机器没装「无线显示器」组件，所以 Miracast 只验到入口、判定与跳转落点；系统接收端自己的全屏界面不在本机可测范围。
- 真实 MV3 扩展加载受浏览器策略限制（见 9.4），popup 验证走的是 shipped 代码 + 五个 `chrome.*` 桩。

## 10. 23:4x–00:0x 轮：「发送端搜不到接收端」打透（#79）

用户报的是 9.4 那套自动发现在真机上没出结果。第一轮判断（Chrome 142 / Edge 143 默认开本地网络访问检查，MV3 扩展页 `fetch` 私网地址没有 `host_permissions` 就被拦）方向对、结论错——本轮把它证伪了一半，并把真因抓了出来。

### 10.1 先证伪：本机复现不了「本地网络访问检查」拦截

- 浏览器版本 `Chrome 156.0.8078.47`，按发布说明这已远过了默认开启的 142。
- 把桩页面跑在**开着**该检查的浏览器里（`.probe/ext-stub-harness.mjs … --lna block`，不再带 `--disable-features=LocalNetworkAccessChecks`）：509 个候选地址全部扫完，**照样**发现 3 个应答（`192.168.0.24:9407`、`172.20.80.1:9407`、`127.0.0.1:9407`），选中的仍是 WLAN 那条。
- 为什么拦不到：这道闸门管的是「公网页面 → 私网/本机」，而桩页面自己就挂在 `192.168.0.24` 上，属同一地址空间；这台纯私网的办公室网络里没有公网侧的起点，本机无法造出被拦的条件。
- 所以 `host_permissions: ["http://*/*"]`（`extension/manifest.json:12`，版本 0.4.0）照加——它是扩展长期需要的加固，10.3 里它还会拿到一条真实作用——但**不能拿它当本次故障的解释**。

### 10.2 真因：扩展是新的，接收端是旧的

- 把用户实际装着的那版重新跑起来（`C:\Users\戎耀会\AppData\Local\horizon meeting\horizon-launcher.exe`，10/26 12:25），在面板里把「浏览器投屏」拨到已开启，再打发现端点：
  - `GET http://192.168.0.24:9407/info` → **`404 Not Found`，空体**（9407 上确实有服务在应答，只是不认识这个路径）；
  - `GET /stream`、`/stream?token=…`、`/nope` → 不响应，curl 超时（exit 28）。
- 发现端点是今天 22:39 那版才加的（9.2）。WebSocket 镜像通道本身 10/24 就上线了，所以旧接收端**能收流、不能被发现**。而 shipped 扩展只认 `/info` 里 `service: "horizon-browser"`，于是 509 个地址全部落空。
- 用 shipped 的 `background.js` + `discovery.js` 直接对着这台旧接收端复现（`.probe/ext-worker-check.mjs`，只桩 `chrome.*`）：`receivers: []`、`selected: null`、`found: false`。**这就是用户看到的「搜不到」。**
- 结论：9.4 那轮把发现做在了新接收端上，用户「先打包」之后并没有装 22:39 的包——两端版本错配，扩展侧没有任何兜底。

### 10.3 修复一：旧接收端也要被发现（404 兜底）

- `probe()`（`extension/discovery.js:52`）不再只看 2xx：`/info` 返回 **404** 时，把它记为一台旧版接收端（`legacy: true`、需要投屏码）。范围收到 404 一条——别的状态码、别的 `service` 一律不列，注释里写明了理由。
- 排序加了一层 `tier()`（`:100`）：自报家门的永远排在「只是在本机镜像端口应答」的前面，混装环境里不会把旧版顶到首选位。popup 的徽章相应多出「旧版，需投屏码」（`extension/popup.js:158`）。
- **假阳性面实测**（`.probe/lan-9407-scan.mjs`）：对本机两块 /24 共 509 个地址做 TCP 连接分类，端口开的只有 3 个——`127.0.0.1`、`192.168.0.24`、`172.20.80.1`，全是这台机器自己的三张面，其余 506 个拒连。兜底规则在这个网络上没有误报对象。
- 修后对旧接收端复跑 shipped 扫描：`receivers` 2 条（WLAN + 虚拟网卡，同一台），`selected: 192.168.0.24:9407`，首个结果 **254 ms** 就上屏。
- `host_permissions` 在本轮拿到的真实作用恰恰在这条兜底上：旧接收端那条 404 **不带 CORS 头**。页面来源读不到它（两种 `--lna` 下都算出 0 条），只有持有 host 权限的扩展才读得到。也就是说这是「只有真扩展才能享受」的一层，桩页面给不了证据——已如实分开记。

### 10.4 修复二：扫描搬进 service worker，结果落进缓存

上一轮的第二个缺陷（popup 一关就断，254 地址扫描跟着死）一并改掉：

- `extension/discovery.js`（新文件）——候选地址、并发 48、单主机 700 ms、每 250 ms 合并进度，worker 与 popup 共用同一份实现。
- `extension/background.js:22` 新增 `scan` 消息，`startSweep()`（`:72`）保证同一时刻只有一趟扫描，进度写进 `chrome.storage.local.discovery`；浏览器启动与扩展更新时预热（`:102`）。
- `extension/popup.js` 从 468 行减到 391 行：自己不再扫，改成先画缓存（`:28`），再每 400 ms 跟 worker 的进度（`:93`），操作者手动选过的那台优先于扫描结果（`userPicked`，`:8`）。
- 三个 JS 全部 `node --check` 通过。

### 10.5 端到端实测（新接收端，真 Chrome 里跑 shipped 代码）

`.probe/ext-stub-harness.mjs 504282 --host 192.168.0.24:9407` —— 这一版把 `popup.js`、`background.js`、`discovery.js` **三个 shipped 文件一起**塞进真浏览器，只桩 `chrome.storage / runtime / offscreen / system.network / tabs / tabCapture`，并用 `localStorage` 冒充 `chrome.storage.local` 的持久性：

| 步 | 读数 |
| --- | --- |
| 三个上下文都在 | `popupHasScan / workerOwnsSweep / discoveryModule` 全 true |
| 扫完 509 个地址 | 7 738 ms（`scanSettled: true`） |
| 接收端列表 | 2 条，`戎耀会 192.168.0.24:9407` 已选中，徽章「需要投屏码」 |
| 谁在发请求 | 509 次 `fetch`，全部是 `/info`，其余 0 次（popup 里根本没有 fetch） |
| worker 写的缓存 | `done: true`、`count: 2`、`selected: 192.168.0.24:9407` |
| 输码即投 | 拼出 `ws://192.168.0.24:9407/stream?token=504282`；不满 6 位被拦（「请输入大屏上显示的 6 位投屏码」）；`12ab34z99` → `123499` |
| 手填兜底 | `ws://10.1.2.3:9407/stream?token=246810`，token 照样带上 |
| **重开 popup** | `Page.reload` 后 250 ms 内：`搜索中…` 且列表已画出 2 条、选中的仍是 `192.168.0.24:9407`（`paintedWhileSearching: true`）——操作者切去看大屏码再回来，不用再等一趟扫描 |
| 圆角未退化 | hero 20px / 主操作 13px / 接收端行 19px / 投屏码输入框 14px |

连上这一头也补了实测：同一个码走真实握手 `ws://192.168.0.24:9407/stream?token=504282` → **54 ms `open`**；不带码、错码都是 `refused`。发现→选中→拼 URL→连上，这四步本轮首尾闭合。

### 10.6 顺手撞出来的一个次要事实

`browser-token` 这次从 `518294` 变成了 `504282`。原因不是「重启换码」失效：`browser_token()` 只接受「6 位纯数字」，而我为了复现 10.2 把 10/26 的旧版应用又跑了一次，它往同一个文件里写了自家格式的长串，新版读回判定非法、于是重新生成并覆盖（`src-tauri/src/receivers.rs:536-551`）。降级跑一次会作废存过的码——现场不常见，但 9.1 那句「重启应用不换码」要加上这个前提。

### 10.7 这一轮的诚实边界

- 真实 MV3 扩展仍然装不上（本机 Chrome/Edge 忽略 `--load-extension`，见 9.4），所以 10.3 的 404 兜底只在 Node 侧 `chrome.*` 桩里验过，页面来源因 CORS 读不到那条 404；用户侧要成立，必须装上 0.5.1 包**并**重新加载扩展（`host_permissions` 是新增权限，必须重装/重新加载才生效）。
- 本轮扫描把同一台机器的虚拟网卡地址（`172.20.80.1`）也列成了第二台，那是收发同机才有的现象：发送端在别的机器上时，它那块面不在被扫网段里。没有做同名合并，因为两台默认同名的接收端合并会丢真机。
- 投屏码在扩展里的输入验证、`{code, receiverHost}` 持久化，跑的是 shipped 代码；抓取与编码链路本轮未动，其真机证据仍是任务 #40 那一轮。

### 10.8 需要用户做的两步

1. 装 `target/release/bundle/nsis/HORIZON Meeting_0.5.1_x64-setup.exe`（0.5.1 的包现在带的是 manifest 0.4.0 的扩展，`extension/` 整个目录进包，`src-tauri/tauri.conf.json:53`）。
2. 在 `chrome://extensions` 里对 HORIZON 投屏点「重新加载」（若提示新增的「访问所有网站」类权限，需确认），然后按 10.5 的路径试：开接收端 → 打开扩展 → 输入大屏上的 6 位码。


## 11. 00:1x–00:4x 轮：欢迎应用的图片/音乐「关掉就恢复」与「只能放一首」（#80 / #81）

对应反馈原文：「欢迎 应用的图片和音乐无法保存，关闭后就恢复了。背景音乐无法播放（好像只能放一首）？」。构建对象是 00:13 的 `target/release/horizon-launcher.exe`（6,540,288 字节，`cargo` 用时 3m51s）与 `HORIZON Meeting_0.5.1_x64-setup.exe`（3,332,279 字节）。

### 11.1 根因不是「没保存」，是「存了两份」

`WelcomePanel` 自己持有 `internalThemes` / `internalActiveId` 两份本地 state，props 传进来的主题只在初始时看一眼——选完图片写进的是组件内部那份，卸载即销毁；而 `App.tsx` 那边 `state.settings` 从没收到过任何写入，所以 `save_state` 落盘的 JSON 里根本没有欢迎数据。这就是「关闭后就恢复」的全部机制：写对了 UI，读错了地方。

改法是把它变成纯受控组件：props 改为必填的 `themes / activeThemeId / onThemesChange / onActiveThemeChange`（`src/components/WelcomePanel.tsx:25-32`），内部两份 fallback state 删掉，所有编辑都走 `updateActiveTheme → onThemesChange → App.updateSettings`（`src/App.tsx:801-809`），于是复用既有的 240 ms 防抖 → `invoke("save_state")` 通路。数据形状进 schema：`WelcomeTheme`（`src/types.ts:30`）+ `settings.welcomeThemes / welcomeActiveTheme`（`src/types.ts:49-50`），读取侧 `normalizeWelcomeTheme()`（`src/lib/state.ts:46`）负责夹取 interval、清洗名称、并把旧版单值 `musicUrl` 迁移成 `musicUrls: [musicUrl]`（`:50-54`），空数组时兜回 `DEFAULT_WELCOME_THEME`（`:247`）。

### 11.2 音乐「无法播放」是 CSP 静默拦截，不是解码失败

音频以 data URL 存（和自定义壁纸同一套），而 `csp` 里只写了 `default-src 'self'`——`default-src` 不覆盖 `<audio>` 的 `media-src`，`data:` 也不在 `'self'` 里，于是 `new Audio(data:...)` 直接违规、`play()` 被拒，且旧代码 `catch(() => {})` 把它咽了，界面上一句提示都没有。`src-tauri/tauri.conf.json:34-35` 两条（`csp` 与 `devCsp`）都补上 `media-src 'self' data: blob:`。

这条修的是发布包，所以证据取在产物上而不是源码上：`.probe/exe-csp.mjs` 在 6,540,288 字节的 exe 里读到 `... img-src 'self' data: blob:; media-src 'self' data: blob:; style-src ...`。行为侧的正面锚点见 11.4——每一次 `Audio.play()` 都 `played: true`，且没触发失败 toast。

顺带把「静默」这个失败模式本身关掉：`play()` 被拒时提示一次「背景音乐无法播放，请重新选择音频文件」（`WelcomePanel.tsx:521-529`，`playFailureShown` 保证只说一次，不逐首刷屏）。

### 11.3 「只能放一首」：单槽 `musicUrl` 换成播放列表

旧结构只有一个 `musicUrl` 字段，且 `audio.loop = true`——第二首永远没有位置。现在 `musicUrls: string[]`，播放规则是**一首时循环、多首时轮播**：`audio.loop = playlist.length <= 1`，`ended` 事件把游标推到 `(i+1) % length`（`WelcomePanel.tsx:514-520`）。切主题时游标归零（`:502-504`），列表被删空时自动回 0（`:507-510`）。选择框 `multiple` 支持一次多选，逐首列出「第 N 首」并可单独移除（`:425-439`）。

### 11.4 实测（全部走产品自身层：CDP `DOM.setFileInputFiles` 驱动真实隐藏 `<input type=file>`，读真实 DOM 与真实 `Audio` 元素）

驱动脚本 `.probe/welcome-check.mjs`（本轮新增）。为观测音频元素，在打开面板前替换 `window.Audio`，记录每个元素的来源长度、`loop`、`play()` 结果——记录的是产品自己创建的元素，不是替身。

| 断言 | 实测 |
| --- | --- |
| 图片选择器接线 | `input[accept="image/*"][multiple]`、`input[accept="audio/*"][multiple]` 两个都在编辑器 DOM 内 |
| 加图反馈 | toast「已添加 2 张图片」；缩略图 1→3，`broken: 0`，舞台三张实图 `1672×941 / 518×841 / 518×841` |
| 加乐反馈 | toast「已加入 2 首背景音乐」；行数 0→2，计数徽标 `图片 3` / `背景音乐 2` |
| 落盘 | `launcher-state.json` → `settings.welcomeThemes[0]` 出现 3 张 data URL（合计 1,417,847 字节）+ 2 条音频（26,534 / 93,438 字节） |
| **跨进程重启后仍在** | `taskkill` 后重新拉起：编辑器仍是 `图片 3` / `背景音乐 2`，舞台 3 张实图自然尺寸齐全；收尾再重启一次，用户自己那张 1672×941 的图 `opacity: 1` 满屏出画，底栏「欢迎 / 1 / 1」 |
| 音频真的在播 | 6 个元素全部 `played: true`、零 CSP 报错；存活元素 `paused: false`、`currentTime 0.0946 / duration 0.3969` 且在推进 |
| 多首轮播（自然 `ended`，非人工派发） | 两条 0.11 s / 0.40 s 的短音轨，采样窗口内 `26534 → 93438 → 26534 → 93438 → 26534 → 93438` 交替建元素约 15 圈，`loop` 全为 `false` |
| 单首循环 | 点行内垃圾桶删到只剩 1 首后：`liveLoop: true`、`livePaused: false` |
| 名称与间隔同样受控 | 名称改成「年会主视觉」→ 输入框、`<select>` 选中项、舞台底栏三处同步；间隔滑到 23 → 标签「切换间隔：23 秒」；两者都进磁盘 |
| 界面无残留英文 | 编辑器叶子节点里匹配 `[A-Za-z]{3,}` 的文本：`[]` |

### 11.5 顺手量到的两个事实与一处收尾

- 磁盘上先出现了一张 1,245,583 字节的用户 JPEG——那是本轮开始前用户自己在跑的应用写进去的，等于修复效果的第一个自然样本（重启后仍在）。测试数据（2 张 PNG、2 条 wav、改过的名称与间隔）已全部通过产品自身的垃圾桶与输入框撤掉，收尾状态回到 `图片 1 / 背景音乐 0 / 欢迎 / 10 秒`，与用户原状一致。
- 播放列表每次变更都会重建 `Audio` 元素并从 0 开始播（`playlist` 引用变化 → 效果重跑）。分钟级音轨下这是「改完列表重头放」，可接受；本轮那 35 个元素是 0.1~0.4 秒测试音轨自然轮播的结果，不是抖动。

### 11.6 用户实测到的一条真数据，以及它牵出的性能账

清理完测试数据后，用户自己在界面上加了一首真歌，磁盘立刻变成：

- `data:audio/mpeg;base64` 12,829,027 字符，`launcher-state.json` 16,723,465 字节。
- 用真实文件在页面里做解码探针（一次性元素，只听 metadata）：`loadedmetadata` → **duration 239.44 s**，即 `media-src` 修复对分钟级 MP3 同样成立。
- 但 `saveState` 是整份状态重新序列化：同尺寸合成载荷实测 `JSON.stringify` 40.4 ms + UTF-8 编码 102.8 ms（14,247,017 字符），加上 IPC 与 Rust 侧回解析，落到帧上就是**每次保存掉两帧长的停顿**。
- 对照实验（同一脚本、同一窗口状态、`rAF` 间隔采样）：开着主题设置抽屉**不改动** 3.2 s → 462 帧、最大间隔 **8.9 ms**、超过 60 ms 的间隔 0 个；改动两次触发两次保存 → 最大间隔 **264.7 / 253.2 ms**。停顿属于保存路径，不属于抽屉的毛玻璃合成。

根因不是欢迎应用独有的：`launcher-state.json` 是**全应用共用**的一份 JSON，`settings.customWallpapers` 里那张壁纸本来就占 2,647,094 字节。也就是说侧边栏拖一次、设备名改一个字、壁纸换一张，都要为这些 base64 付一次主线程序列化。

修法（任务 #82，00:48 构建）：媒体旁置。`src/lib/bridge.ts` 在保存前把 `images / musicUrls / customWallpapers` 里的 `data:` 项挪进同目录的 `welcome-media.json`，状态里只留 `media:<key>` 引用；加载时反向回填（引用找不到就丢弃该项，而不是画出一个坏块）。Rust 侧只多两个命令（`src-tauri/src/lib.rs:67` 与 `:78`，旁置文件路径由 `:61` 定；注册于 `:1231-1232`），保存链被串起来以保证「先写媒体、后写状态」的顺序——反过来才会出现引用悬空。旧文件里内联的 base64 不需要迁移脚本：第一次保存自动完成拆分。

拆分后的复测：

| 指标 | 拆分前 | 拆分后 |
| --- | --- | --- |
| `launcher-state.json` | 16,723,465 B | **1,815 B**（`customWallpapers` 22 B、`welcomeThemes` 107 B，都是引用） |
| 一次设置的 `longtask` 主线程阻塞 | 帧间隔 253–265 ms | **0 条 long task**（两次保存采样窗口内为空） |
| 媒体文件是否被无关设置连带重写 | 每次都写 | **mtime 不变**（`mediaRewritten=0s`，同窗口内状态文件写了 29 s 前的那份） |
| 壁纸回归 | — | `.ambient__wallpaper` 的 `background-image` 仍是 2,647,097 字符的 `data:image`（引用已回填） |
| 欢迎图/歌回归 | — | 重启后 `1672×941 · opacity 1`；音频 `data:audio/mpeg · paused:false · currentTime 2.41↑ · duration 239.44 · loop:true` |

### 11.7 这一轮的诚实边界

- 测试音轨用的是 `C:/Windows/Media` 下的短 wav（0.11 s / 0.40 s），便于在秒级窗口内观察轮播；分钟级真实音乐的自然 `ended` 未跑满一圈，靠的是同一行 `(i+1) % length` 与人工删除后的 `loop` 断言。用户那首 239 s 的歌验的是解码与续播，不是自然收尾。
- 音频是否**出声**无法在本机断言（无采集回环），验的是元素 `paused/currentTime/duration` 与 `play()` 决议——即「未被 CSP 拦截且解码成功」。扬声器链路那一头仍属人耳确认范围。
- 帧间隔采样受窗口焦点影响（拆分那一轮 `rAF` 只跑到 ~55 fps），所以 253–265 ms 与 8.9 ms 只作同法对照用；绝对值以 `longtask` 为准，而 `longtask` 只有拆分后的数据——拆分前那一版构建已被覆盖，无法回补同法基线。
- 本轮应用被外部关闭 3 次（`.probe/launcher-stdout.log` 无 panic、事件日志无该进程错误），因此末段把「开面板→开编辑器→清理→截图」合并进单个 CDP 会话跑完；这不影响结论，但说明测量是在有人同机使用应用的条件下做的。
- 旁置只解决「无关设置不再搬运媒体」。真正加/删一张图或一首歌时，仍要整份 `welcome-media.json` 重写一次（当前 14 MB），那一下还是百毫秒级；再往下就是逐文件存储（每条媒体一个文件 + 只写变化的那个），本轮未做。

## 截图

- `verify1.png`：首页（最终构建）- `verify2.png`：设置抽屉「批注应用」章节
- `verify4.png`：办公应用面板（旧版，点击即启动）
- `verify5.png`：未设置默认时的首次选择面板（行内「设为默认」）
- `verify6.png`：设置抽屉「办公应用」章节
- 8.10 的镜像画面未导出为文件，证据以窗口内可读文本记录：面板「镜像帧 1273，关键帧 1」与舞台顶栏「正在接收 · AirPlay 镜像 1920×1080 · 已配对」，舞台画布内容为 Mac 桌面本身。
- `verify7.png`：点击 Office 直启默认应用（toast，无面板）
- `verify8.png`：修复后的首页与侧边栏（7 个工具可点）
- [`shots/airplay-receiver-on.png`](shots/airplay-receiver-on.png)：AirPlay 接收已开启，广播名与监听地址（第 7 节）
- [`shots/airplay-stage-20-2x.png`](shots/airplay-stage-20-2x.png)：20:2x 构建的全屏舞台，Mac 镜像 1920×1080 实机画面（Safari 打开 `ishohealth.com`，正文与小字边缘清晰，顶栏「正在接收 · AirPlay 镜像 1920×1080 · 已配对」）
- [`shots/airplay-drawer-streaming.png`](shots/airplay-drawer-streaming.png)：`is-streaming` 挂上时抽屉的观感——遮罩由毛玻璃退成纯色，卡片、文字与配色不变
- [`shots/airplay-card-blank-after-stage.png`](shots/airplay-card-blank-after-stage.png)：8.15 缺陷证据——引擎侧「正在镜像 · 1920×1080 · 已配对」，卡片却停在「等待 iPhone、iPad 或 Mac 开始镜像」
- [`shots/airplay-card-after-stage-fixed.png`](shots/airplay-card-after-stage-fixed.png)：8.16 修复后——退出全屏卡片继续出画（Mac 上的网页清晰可读），面板「正在镜像 · 1920×1080 · 已配对 · 镜像帧 3583，关键帧 1」
- [`shots/card-code-22-30.png`](shots/card-code-22-30.png)：9.1 卡片预览窗的 6 位投屏码字卡（「投屏码 / 5 1 8 2 9 4 / 在浏览器扩展里输入这 6 位数字」），修前从数字缝里透出来的那条提示已消失
- [`shots/stage-code-2026-10-29.png`](shots/stage-code-2026-10-29.png)：9.1 全屏舞台的投屏码（补上 `.cast-stage__code` 排版前的样子，数字与舞台自带提示叠在一起）
- [`shots/stage-code-final.png`](shots/stage-code-final.png)：9.1 最终构建的全屏舞台——152px 数字居中、顶部光晕、顶栏「等待投放 · 浏览器投屏 · 局域网直连」，预览盒那条「等待…」提示已被压掉
- [`shots/ext-stub-idle.png`](shots/ext-stub-idle.png)：9.4 扩展 popup 待命态（shipped `popup.html`/`popup.js` 跑在真浏览器里、只桩 `chrome.*`），圆角与接收端列表
- [`shots/ext-stub-live.png`](shots/ext-stub-live.png)：9.4 扫描进行中的 popup——已发现的接收端边扫边出现，投屏码输入框与「已选」状态
- [`shots/ext-stub-idle-allow.png`](shots/ext-stub-idle-allow.png)：10.5 待命态——worker 扫完写进缓存的 2 台接收端、已选中 WLAN、徽章「需要投屏码」、圆角未退化
- [`shots/ext-stub-live-allow.png`](shots/ext-stub-live-allow.png)：10.5 输码后的 popup（「已选 戎耀会，投屏码 504282。」）
- [`shots/ext-stub-idle-block.png`](shots/ext-stub-idle-block.png) / [`shots/ext-stub-live-block.png`](shots/ext-stub-live-block.png)：10.1 那轮对照——本地网络访问检查**开着**时的同一界面，用作「本机复现不了拦截」的证据图
- [`shots/welcome-snapshot.png`](shots/welcome-snapshot.png)：11.4 主题设置抽屉——用户自己那张 1672×941 的欢迎图 + 本轮加进去的 2 张测试图（缩略图 3）、背景音乐 2 首逐行列出，标签全中文
- [`shots/welcome-persisted-stage.png`](shots/welcome-persisted-stage.png)：11.4 跨进程重启后满屏出画的欢迎图（顶栏齿轮/退出、底栏「欢迎 · 1 / 1」与播放控制）——「关闭后就恢复」已不成立
- [`shots/welcome-editor-restored.png`](shots/welcome-editor-restored.png)：11.5 清理测试数据后的编辑器（图片 1 / 背景音乐 0 / 间隔 10 秒），与用户原状一致
- [`shots/welcome-after-split.png`](shots/welcome-after-split.png)：11.6 媒体旁置后重启的欢迎界面——图与歌都从 `media:` 引用回填后正常出画/续播（用户自己的欢迎主视觉，1672×941 满屏）


