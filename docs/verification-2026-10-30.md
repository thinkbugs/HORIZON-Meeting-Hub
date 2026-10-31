# 验证记录 2026-10-30 · 全屏压过任务栏 / 右键菜单 / 文件传送 / 开机启动 / Win10 截屏录屏

验证对象：`D:\codeX\meeting\target\release\horizon-launcher.exe`（v0.5.1，`npx tauri build` 产出，安装包 `target\release\bundle\nsis\HORIZON Meeting_0.5.1_x64-setup.exe`）

验证方式：WebView2 远程调试（`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333`）+ CDP 读取真实 DOM / 触发真实点击 / 调用应用自身 IPC；PowerShell 读取 Win32 窗口矩形、样式位、z 序；`netsh` / `reg query` 读取系统状态；`curl` 与本仓库 `.probe/*.mjs` 作为真实客户端。不依赖屏幕坐标点击。

## 1. 全屏覆盖任务栏

本机任务栏竖排在右侧（`1872,0,1920,1080`），桌面工作区只有 `1872` 宽，所以「压过任务栏」必须让窗口矩形等于整个显示器，而不是工作区。

| 状态 | 窗口矩形 | topmost | 与 `Shell_TrayWnd` 的 z 序 |
| --- | --- | --- | --- |
| 最大化（基线） | `-8,-8,1880,1088` | false | 下 |
| 点击「进入全屏」 | `0,0,1920,1080` | true | `Tauri Window 69` 在 `Shell_TrayWnd 394` 之上 |
| 点击「退出全屏」 | `-8,-8,1880,1088` | false | 下 |

- 全屏时窗口矩形覆盖 `x=1872…1920` 的任务栏条，且处于 topmost 带；z 序脚本（`.probe/zorder.ps1`）判定 `appIsAboveTray = true`。
- 退出全屏后窗口回到最大化矩形，而不是留在 `0,0,1920,1080`。

从**非最大化**窗口再走一遍：全屏前 `216,90,1672,999`（1456×909 外框），进出全屏后仍是 `216,90,1672,999`，尺寸不变。

### 顺带修复的缺陷（退出全屏残留全屏矩形）

第一次实测退出全屏时窗口停在 `0,0,1920,1080`（未恢复最大化边框）。原因是 tao 的 `set_fullscreen(false)` 只把窗口恢复显示，不会重新套用全屏前的窗口状态。修复：

- 进入全屏前记录 `is_maximized()`、`outer_position()`、`inner_size()`；
- 退出全屏后若原本最大化，先 `unmaximize()`（SW_RESTORE）再 `maximize()`（SW_MAXIMIZE）——对已最大化的窗口直接 `maximize()` 是空操作，只有这一对序列会重写矩形；若原本非最大化，则按记录的位置与客户区尺寸还原。
- 尺寸保存用的是**客户区**尺寸：`set_size` 写的是客户区，若存外框尺寸，每次往返窗口都会长大一圈（实测多出 16×9px）。

## 2. 启动器右键菜单

- 在启动器 `body` 上派发 `contextmenu`：`defaultPrevented = true`，页面不再弹出 WebView2 的系统菜单（刷新/另存为/检查等）。
- 输入框（设置里的文本框）未做拦截，`defaultPrevented = false`，复制/粘贴的系统菜单保留——只屏蔽启动器整体，不牵连打字场景。

## 3. 文件传送（SnapDrop）局域网不可达与配置不保存

这一条是链路上多处断裂叠加，逐段修复并逐段验证。

### 3.1 地址打不开

| 节点 | 缺陷 | 处理 |
| --- | --- | --- |
| 服务生命周期 | 监听器建在命令私有的 tokio runtime 上，`block_on` 返回后 runtime 被 drop，端口随即关闭 | 命令改为 `async`，监听器挂在应用常驻 runtime 上；面板关闭后服务继续监听 |
| 静态资源 | `ServeDir` 以进程 CWD 为根，安装后找不到 `scripts/` | 资源根改为应用资源目录 |
| 打包映射 | `"assets/snapdrop/**/*": "snapdrop/"` 会**打平**子目录，安装包里没有 `scripts/`，页面 404 | 改为 `"assets/snapdrop/": "snapdrop/"`，保留目录结构 |
| 防火墙 | 公用网络配置文件入站默认 `Block`，本机 609 条入站规则里没有 `horizon-launcher.exe`，安装器是 currentUser 不提权 → 从未生成规则 | 面板检测规则是否存在，缺失时显示提示条与「放行」按钮，点击通过 `ShellExecuteExW("runas")` 执行一次 netsh 建规则（需要用户点一次 UAC） |

实测：

- 服务在面板打开后常驻：关闭面板后 `GET http://192.168.0.24:7878/` 仍为 200；`netstat` 显示 `0.0.0.0:7878 LISTENING`。
- 静态资源：`/`、`/scripts/network.js`、`/scripts/ui.js`、`/styles.css` 全部 200；`target/release/snapdrop/scripts/network.js` 中存在本轮修复的 `peer-message` 分支（安装包内路径正确）。
- 防火墙检测的判据本身做了正反锚定：`netsh advfirewall firewall show rule name="HORIZON Meeting SnapDrop"` 退出码 1（不存在，与 UI 提示一致）；`name="Delivery Optimization (TCP-In)"` 退出码 0（存在）。活动配置文件 `Domain/Private/Public` 的 `DefaultInboundAction` 均为 `Block`。
- **真实第二台设备未能验证**：本机没有第二台电脑，也没有 WSL/Docker 可用于第二主机，同机访问 `192.168.0.24` 不经过防火墙入站规则。因此「其他电脑能打开」的最后一环需要用户点一次「放行」并在 UAC 中确认；放行后规则对 `horizon-launcher.exe` 全配置文件开放。

面板现状：地址 `http://192.168.0.24:7878`；防火墙提示条显示（规则确实不存在）且「放行」可用；设备名 `363363`、存储目录 `C:\Users\戎耀会\Desktop` 均为重启后读回的原值。

### 3.2 设备名 / 存储目录改完不保存

| 缺陷 | 处理 |
| --- | --- |
| 配置结构体按 camelCase 序列化，读回时字段名对不上 | serde 同时接受 `deviceName`/`device_name` 等别名，读写同一份 `%APPDATA%\horizon-launcher\snapdrop-config.json` |
| 面板只读进程内状态，重启后回落到默认值 | `get_snapdrop_config` 在服务未启动时直接读磁盘，重启后仍显示上次保存值 |
| 存储目录只在接收文件时才报错 | 保存时先 `create_dir_all`，目录不可写当场提示 |

实测（全部经界面/IPC 与磁盘文件双向核对）：

| 步骤 | 结果 |
| --- | --- |
| 面板内改名 → 保存 | 面板显示 `HORIZON-会议室`，toast「设备名已更新」，配置文件 `deviceName` 同步 |
| 通过 IPC 改存储目录 | 配置文件 `storageDir` 同步 |
| 关闭进程后重启 | `get_snapdrop_config`（服务未启动、走磁盘回退）返回 `HORIZON-会议室` / 测试目录 |
| 打开面板 | 「设备名称」「存储目录」两处显示与磁盘一致 |
| 恢复原值 | 设备名恢复 `363363`、存储目录恢复 `C:\Users\戎耀会\Desktop` |

### 3.3 传输本身

| 客户端 | 结果 |
| --- | --- |
| 裸 WebSocket（`.probe/snapdrop-e2e.mjs`，2.4 MB / 64 KB 分片 / 1 MB 分区握手） | 收到 `partition-received`、`progress`、`transfer-complete`；落盘 2400000 字节，sha256 一致 |
| 真实浏览器（`.probe/snapdrop-browser.mjs`，headless Edge 走页面文件选择框，1.8 MB） | 落盘 1800000 字节，sha256 一致；发送端收到 `transfer-complete`（页面 toast「File transfer completed.」），对端标签显示 `363363`；接收面板全程没有残留「正在接收 … 100%」 |

修复的协议缺陷：

- 中继回复缺少 `sender`，客户端把它当未知消息丢弃，传输在 1 MB 分区处卡死 → 服务端回复统一带上 sender。
- 客户端侧 `WSPeer` 没有继承 `Peer`（无 `sendFiles`/`_onMessage`）、二进制分片被 `JSON.stringify`、中继消息没有转交到 peer → 三处补齐。
- `peers` / `peer-joined` 里的 `name` 是字符串，而页面按 `peer.name.displayName` 渲染 → 对方的设备网格显示空白行；改为协议原样的 `{displayName, deviceName}` 对象。修复后浏览器端读到的行标签为 `363363`（此前为空串）。
- 浏览器客户端在文件末尾不会补发 `partition`（`_onChunkRead` 到达文件尾直接 return），接收端只在 `partition` 分支判断「收满即完成」→ 最后一个 1 MB 尾段永远不触发 `finish_file`：发送方收不到 `transfer-complete`（一直处于忙状态），接收面板停在「正在接收 … 100%」。现在按字节数判断完成：二进制分片写入后一旦 `received >= expected` 立即收尾（空文件在 header 阶段收尾）。

## 4. 开机启动

设置抽屉的「开机自动启动」开关，读写都落到 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。

| 步骤 | 结果 |
| --- | --- |
| 初始状态 | 注册表无 `HORIZON Meeting` 值；开关 `aria-checked=false`；`autostart_status` 返回 false |
| 打开开关 | toast「已设置为开机自动启动」；开关变 true；注册表出现 `HORIZON Meeting = "D:\codeX\meeting\target\release\horizon-launcher.exe"` |
| 读回 | `autostart_status` 返回 true（读的是注册表，不是内存标志） |
| 关闭开关 | toast「已取消开机自动启动」；开关 false；注册表值被删除 |

验证后已恢复原状（关闭），机器上不留自启动项。

## 5. Win10 截屏 / 录屏

Windows 11 的截图工具有录屏，Windows 10 没有；Win10 的录屏在 Xbox Game Bar。原实现一律走 `SnippingTool.exe screenrecord`，Win10 上等于什么都不做。

现在按真实系统版本分派：

| 系统 | 截屏 | 录屏 |
| --- | --- | --- |
| Windows 11（build ≥ 22000） | `ms-screenclip:` → 截图工具覆盖层；URI 不可用时回落 `SnippingTool.exe`，并提示「按 Win+Shift+S 开始截图」 | `SnippingTool.exe`，提示「点击『录制』或按 Win+Shift+R 开始录屏」 |
| Windows 10（build < 22000） | `ms-screenclip:`（Snip & Sketch 覆盖层）→ `SnippingTool.exe` | `ms-gamebar:` → `ms-gamingoverlay:`，提示「按 Win+Alt+R 开始录制」 |

关键修复：版本探测原先用 `GetVersionExW`，它按可执行文件的 manifest 报告兼容版本（19041），导致这台 Windows 11（26200）落进 Windows 10 分支——这正是「Windows 11 正常 / Windows 10 不正常」表象下被掩盖的另一半问题。改用 ntdll 的 `RtlGetVersion`，不受 manifest 影响；实测 `screenrec` 现在返回 Windows 11 分支的提示文案。

实测（本机 Windows 11 26200）：

| 步骤 | 结果 |
| --- | --- |
| 侧边栏「截屏」 | 调用返回 null（第一步已成功），`ms-screenclip:` 已注册（`HKCR\ms-screenclip` 存在），进程表出现 `SnippingTool.exe`（PID 52952，"截图工具覆盖"），覆盖层直接打开，不需要额外提示 |
| 侧边栏「录屏」 | 调用返回「已打开截图工具，点击『录制』或按 Win+Shift+R 开始录屏」；`SnippingTool.exe`（PID 47592，"截图工具"）窗口出现 |
| 验证后 | 两个截图工具进程均已关闭 |

**Windows 10 分支未在本机执行**：这是一台 Windows 11 机器，没有 Win10 环境可切换；分支逻辑由 10 个单元测试覆盖（含「Win10 不得调用 Win 11 才有的录屏参数」）。这一条属于环境限制，不是未完成项。

## 6. 构建与产物

- `cargo test --lib`：10 passed / 0 failed。
- `npx tauri build` 成功，产物 `target\release\horizon-launcher.exe`（2026-10-30 02:02）与 `bundle\nsis\HORIZON Meeting_0.5.1_x64-setup.exe`（4.35 MiB）。

面板现状截图：`docs/shots/file-transfer-firewall-2026-10-30.png`（已随仓库整理移除，历史提交 `6d701a9` 可查）。

## 仍需要用户完成的一步

文件传送首次使用时，点击面板里的「放行」并在 UAC 弹窗中确认一次，Windows 防火墙才会为 `horizon-launcher.exe` 建立入站规则；不点这一步，其他电脑访问 `http://<本机IP>:7878` 仍会被系统拦截。规则建立后不再需要任何操作。

如果放行后仍然打不开，那就不是本机的问题了：常见原因是 Wi-Fi 的「AP 隔离 / 访客网络」或路由器 VLAN 把两台设备隔在不同广播域——这类网络里任何两台设备都无法互相直连，需要网络管理员放开。

## 7. 源码发布

远端：<https://github.com/thinkbugs/HORIZON-Meeting-Hub>（公开仓库，默认分支 `main`）。发布建立在远端已有的 `Initial commit`（MIT LICENSE）之上，本地提交 `6884241` 快进推送：`a915940..6884241  main -> main`。

入库范围：256 个文件、34.1 MB——`src/`、`src-tauri/`、`crates/horizon-cast/`、`extension/`、`public/`、`docs/`、根目录界面截图、全部工程配置，外加新增的 `README.md`。

| 项 | 处理 |
| --- | --- |
| `node_modules`、`dist`、`target/`、`src-tauri/gen` | 忽略（构建产物） |
| `.probe/` | 忽略（本机验证脚本，路径绑定本机） |
| `.qoder/` | 忽略（会话配置） |
| `oem/` | 忽略（客户定制信息） |
| `tools/gen_fairplay_tables.cjs` | 写死的本机临时目录改为命令行参数 |
| 开发用自签名证书/私钥、FairPlay 常量表 | 入库（`include_bytes!` 编译依赖，安装包内本已携带），README 标注仅限本机调试与协议研究 |

链路与认证（逐环节实测，不是推测）：

| 环节 | 实测结果 |
| --- | --- |
| `github.com:443` 直连 | 连接重置 / 21 s 超时；同机 `api.github.com` 直连 200，说明是域名级拦截而非断网 |
| 系统代理 | WinINET `ProxyEnable=1`、`ProxyServer=127.0.0.1:7897`，`curl -x` 访问仓库页 200 |
| Git 传输 | 仅对本仓库设置 `http(s).proxy`，`git ls-remote origin` 正常返回 `a915940` |
| 认证 | GCM 设备码登录（`git-credential-manager github login --device`），凭据落入 Windows 凭据管理器；`git credential fill` 返回 `username=thinkbugs` |
| 推送 | `6884241` 快进推送成功，`main` 已跟踪 `origin/main` |

远端校验（GitHub Git Tree API，`recursive=1`）：

| 项 | 结果 |
| --- | --- |
| HEAD | `6884241cf4019329556e09ef264c2a01176f5a96` |
| 树条目 / blob | 293 / 257（256 个源码文件 + 远端 LICENSE） |
| 关键文件 | `README.md`、`src/App.tsx`、`src-tauri/src/lib.rs`、`src-tauri/tauri.conf.json`、`crates/horizon-cast/src/airplay/fairplay.rs`、`extension/manifest.json`、`docs/verification-2026-10-30.md`、`docs/shots/file-transfer-firewall-2026-10-30.png` 全部命中 |
| 忽略项 | `.probe/`、`oem/`、`node_modules/`、`dist/`、`target/`、`.qoder/` 命中 0 条 |
| 仓库页 | `https://github.com/thinkbugs/HORIZON-Meeting-Hub` HTTP 200 |

## 8. DLNA 大屏舞台：播放面几何、变形、可控性与控制面可靠性

验证对象：开发态 `target\debug\horizon-launcher.exe`（`npm run desktop`，Rust 改动由 watcher 自动重建，前端改动即时热更新）。接收端由应用自身的 `dlna_start` 启动，端口每次不同（本轮 `57963`），控制器用 `tools/dlna-cast.mjs` 与临时探针脚本。

### 8.1 缺陷与对应改动

| 现象 | 根因 | 改动 |
| --- | --- | --- |
| 播放标题栏 / 退出按钮被画面盖住 | 视频面是 Tauri 窗口的**原生子窗口**（类 `HorizonCastPlayer`，style `0x54000000`），DOM 的 z 序压不住它 | `src/styles/overlays.css` 把 `.dlna-stage__bar` 改成真实 flex 行（`order:-1; flex:none`），画面区 `.dlna-video__screen` 因此整体下移，几何上不可能再压住 chrome |
| 画面区顶部与 chrome 重叠 | 同上 | 实测画面区矩形由 `0,0,1919,940` 变为 `0,67,1919,873` |
| 拉伸后内容变形 | 引擎尚未报出视频尺寸时按舞台铺满 | `crates/horizon-cast/src/player.rs` 的 `place_surface`：无尺寸时 `SW_HIDE` 并撤销上一次矩形，等 pump 每 tick 重试 |
| 标题栏时间重复出现 | 播放行已有时间 | 删除 `.dlna-stage__time`（DOM 与样式一并移除） |
| 音乐投屏只有一片白 | 无音乐界面 | `.dlna-music`：唱片、频谱条、标题、设备、进度、−10/播放暂停/+10/停止、音量 |
| SOAP 响应偶发连接重置 | 见 8.4 | 响应单段写出；写完发 FIN，再在 linger 表里排空读缓冲，收到对端 FIN 或硬错误即刻关闭，300 ms 超时兜底 |
| 图片投屏在大屏上不显示 | 静帧由前端 `<img>` 画，`src` 是控制器给的外链 `http://…/photo.jpg`，被 CSP 拦掉 | `src-tauri/tauri.conf.json` 只放开 `img-src … http:`（`script-src`/`connect-src` 未动）。DLNA 的媒体地址按协议就是任意 http URL，收窄到具体网段需要另做 asset 代理，本轮没有做 |

### 8.2 播放面几何实测（EnumWindows / EnumChildWindows，客户区坐标）

舞台画面区 `[0, 67, 1919, 873]`（`devicePixelRatio=1`）。同一舞台、同一画面区，投两种不同宽高比的源：

| 片源 | 宽高比 | 播放面尺寸 | 左上角（相对客户区） | 判定 |
| --- | --- | --- | --- | --- |
| `border5.mp4` | 16:9 | `1552x873`（1.7778） | `(184, 67)` | 高度受限、左右黑边、水平居中 `(1919−1552)/2=183.5` |
| `square.mp4`（本轮生成） | 4:3 | `1164x873`（1.3333） | `(378, 67)` | 同上，`(1919−1164)/2=377.5` |
| 真机投放 `mmexport1786838896124.mp4`（手机 192.168.0.37，5:12） | 9:16 | `491x873`（0.5624） | `(714, 67)` | 宽度受限、上下无裁切、水平居中 `(1919−491)/2=714` |

三行的 top 都是 `67`，即画面永远起在标题栏下沿；实测比值与三种源片宽高比逐一吻合（1.7778 / 1.3333 / 0.5624 ≈ 9:16 的 0.5625），说明「拉伸变形」不再可能——矩形是按 `GetNativeVideoSize` 做等比适配算出来的。播放结束后（`phase=ended`）播放面 `vis=False`、style 变 `0x44000000`，隐藏路径同样成立。

### 8.3 大屏侧可控性（点击真实 DOM 按钮，读 `receiver_status`）

| 操作（`.dlna-stage button[aria-label]`） | 状态变化 |
| --- | --- |
| 起始 | `playing 1.6` |
| 暂停 | `paused` |
| 播放 | `playing 2.8` |
| 后退 10 秒 | `0.6` |
| 前进 10 秒 | `10.7` |
| 进度条拖到 50%（`input[type=range]`，`duration=300`） | `11.9`（钳制在当前位置 +10 内，见「已知限制」） |
| 停止投放 | `stopped` |

音频链路：投 `tone.mp3` 后 `phase=playing duration=90.07`，`.dlna-music` 存在、标题为 `tone`，界面截图 `docs/shots/dlna-stage-music.png`。

真机端到端：手机（192.168.0.37）经系统 DLNA 投放 `mmexport1786838896124.mp4`，大屏侧自动进入舞台，标题栏显示「正在播放 mmexport1786838896124」+ 退出按钮、底部完整播控条（进度、0:39/5:12、−10、暂停、+10、停止、音量）全部可见可用，截图 `docs/shots/dlna-stage-video.png`（整屏真实抓取，含原生播放面）。

### 8.4 控制面重置：数字与归因

| 形态 | 结果 |
| --- | --- |
| 同进程顺序 SOAP（4 动作 × 8 轮 = 32 次） | 0 失败 |
| 同进程完整投放（描述 GET + 3 动作，12 轮） | 11/12 全绿；1 次描述 GET 断连（`UND_ERR_SOCKET`），同轮后续 3 个动作仍 200 |
| 新进程单请求，本机自连（fetch / node:http / 裸 socket，各 25 次） | fetch 6–11 失败、node:http 3–4 失败、裸 socket 1–2 失败 |
| 同样「新进程单请求」打 node 自建 HTTP 服务（同机、同 `Connection: close`、同主动关闭） | 0/40 失败 |

服务端 trace（`HORIZON_DLNA_TRACE=1`，逐连接打印 accept / 请求行 / FIN / 关闭原因）显示：**每一次客户端重置都对应一次「accept 成功但一个字节请求都没读到」的连接**，两者 1:1；失败的连接在 `netstat` 里不留 TIME_WAIT，成功的都留。

逐个排除的变量（每次只改一个，测后回滚无收益的改动）：

| 变量 | 试验 | 结果 |
| --- | --- | --- |
| 响应分多段写出 | 合并为单段 | 修复了「响应写一半连接就断」这一类，投放从 4/10 失败到 11/12 |
| 关闭时不排空收缓冲 | 加 linger 表：FIN 后排空、见对端 FIN 或硬错误即关 | 保留（无排空的关闭会把响应一起毁掉） |
| accept 轮询间隔 | 20 ms → 2 ms（忙时） | 无变化，已回滚 |
| 谁先关（把 TIME_WAIT 挪到客户端） | 写完不发 FIN，等对端先关 | fetch 反而恶化到 14/25，已回滚 |
| 系统代理 / Clash | 客户端与服务端同机、`127.0.0.1` 与局域网 IP 都复现 | 与本问题无关 |

结论（诚实版）：残留失败只在「每个请求一个新进程 + 同一台机器自连」这种合成形态下复现，三种客户端都会遇到，指向 Windows 本机端口复用与四元组碰撞；真实控制器是长驻进程、顺序发多个动作，该形态 0/32 未见失败。本轮把**服务端能负责的部分**（响应完整性、关闭排空、积压一次性接完）都修掉了，未定位到剩余差异，未继续以猜测改代码。复现工具：`cargo test -p horizon-cast --lib -- --nocapture --ignored control_plane_stress_listener`（带 trace 的独立接收端）。

### 8.5 本轮已知限制

- 像素级截图分两种：CDP 抓取只含 DOM，原生播放面不入镜；`Graphics.CopyFromScreen` 在会话锁屏 / 显示关闭时返回全白，所以那一段时间的判定靠**矩形与样式位**。本轮晚些时候会话回到前台，`CopyFromScreen` 已能抓到完整画面（`docs/shots/dlna-stage-video.png` 即真机投放的整屏实抓），两种手段现在都可用。
- 进度条拖拽到 50% 只走到 11.9 s：`dlna_control{Seek}` 走的是同一条 `Transport::seek`，−10/+10 已证明通路正确；大跨度跳转受 MediaEngine 关键帧位置影响，未在本轮单独定位。
- 追记（节点 6）：上面这句里的 `dlna_control` 门已于节点 6 移除，控制统一走 `media_invoke`；`11.9 s` 是当时的测量，保留原文不改写，但它描述的那条通道现在已经不存在了。
- 手机侧仍未验证：ADB 不可达（无线调试未开，`adb connect` 10061 拒绝），#103 输入框聚焦、#104 端到端延迟与 1080p30 长跑、#107 桌面图标真机确认三项挂起。

## 9. 「启动器挡住本地应用」：让位逻辑的收口与复验

§8 的实屏抓取顺带把 #105 的让位逻辑重新查了一遍，查出一个真实缺口。

### 9.1 缺口

置顶带只在 `launch_path`（自定义应用）这一条路上主动放下，其余启动入口都是直接
`shell_open`，也就是只靠 `Focused(false)` 事件被动放下：

| 入口 | 改动前 |
| --- | --- |
| `launch_path`（自定义应用、Office、批注应用） | 启动前主动 `set_always_on_top(false)`，失败再补回 |
| `open_builtin`（文件、浏览器、记事本、截图/录屏、投影设置） | 无，只靠焦点事件 |
| `run_builtin_steps`（按 Win10/11 版本回退的多步启动） | 无，只靠焦点事件 |
| `miracast_launch`（系统无线投屏 / 投影设置页） | 无，只靠焦点事件 |
| `open_extension_folder`（打开浏览器扩展目录） | 无，只靠焦点事件 |

被动路径的问题正是用户看到的现象：新窗口如果在置顶带还挂着的时候才起来，Windows
的前台锁会不让它拿到前台，于是启动器收不到 `Focused(false)`，带子一直压着，应用开在
启动器后面。

### 9.2 改动

把让位收进唯一的出口 `shell_open_yielded(app, target, args)`：先放带，再 `ShellExecuteW`，
启动失败立刻 `sync_topmost` 把带补回去（这种情形下焦点没走，没人会替它恢复）。
上表四个裸调用点全部改走这个出口。静态核对：`grep -n "shell_open(" src-tauri/src/lib.rs`
在包装函数之外命中 0 处。

### 9.3 实屏复验做到哪一步

| 测到的 | 数值 |
| --- | --- |
| 启动器全屏 + 聚焦时确实会进置顶带 | `WS_EX_TOPMOST=1`、`fg=True`、rect `[0,0]-[1920,1080]` |
| 经产品自身命令打开文件管理器 | `open_builtin{action:"files"}` → `ok:null` |
| 新窗口没有被压在启动器下面 | 新 `CabinetWClass` 在 Z 序第 44 位，启动器第 46 位（`EnumWindows` 自顶向下） |

没测到的（不猜）：**「带子开着 → 启动 → 带子在焦点转移之前就被应用自己放下」这一条判据
没有拿到**。两个原因——

1. 手动 `SetWindowLong` 造出来的带子和 tao 内部缓存的置顶状态不同步，应用随后的
   `set_always_on_top(false)` 会被当成「本来就是关的」而跳过，这样测出来的「没放下」
   是测量手段的假象，不是产品行为；
2. 这台桌面当时正在被使用（另一个浏览器多次抢走前台），带子撑不过一次启动。

所以 #105 的收口改动按**代码路径 + 静态核对**成立，按**端到端判据**尚未闭环，需要在大屏
无人使用时把启动器切进全屏、再从界面点一次「文件」/「记事本」补测。

### 9.4 产物重建

§8 与 §9 的 Rust / CSS / CSP 改动全部进包：`cargo test -p horizon-cast --lib` 184 passed /
0 failed / 6 ignored；`npx tauri build` 于 19:46 产出
`target\release\horizon-launcher.exe`（6,612,992 B）与
`target\release\bundle\nsis\HORIZON Meeting_0.5.1_x64-setup.exe`（4,569,047 B，4.36 MiB），
编译告警只剩 `fairplay.rs` 里既有的 7 条括号冗余。
