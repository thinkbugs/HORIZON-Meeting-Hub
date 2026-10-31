# 验证记录 2026-10-30 · 媒体投屏链路（片库 / 队列 / 媒体 HTTP）

本文件按**链路节点**记录，一个节点一节：节点要证明什么、怎么证的、结论，以及**没打到的部分**。后续节点（原生 API、DLNA 兼容、大屏 UI、扩展、安卓）在同文件续写，不另开文件。

验证方式：Rust 单元/集成测试（`cargo test`）、真实 TCP 套接字上的 HTTP 报文（`tokio::net::TcpStream` 手写请求，走完整 hyper+axum 栈，不是 mock）、`curl` 打真实安装包。

---

## 节点 0 · 分层决定（不是测试，是前提）

投单张图片/视频/音乐而不是整屏，主机侧必须有一个**片库 + 队列**：发送端只负责把文件送到主机或给出地址，播放、排序、续播由主机完成。理由：

- DLNA 控制器只会 `SetAVTransportURI` 一个地址，把「一个地址」映射成「一个条目的队列」，第三方兼容性不缩水；
- 发送端（浏览器/手机）休眠或关页面时，URL 直投会断流，落在主机片库里的文件不会；
- 只有可寻址的条目才谈得上重排、重播、断线后状态仍在。

由此确定两处落点：片库目录 `%APPDATA%\horizon-launcher\media`（与文件传送的 `Desktop\MeetingHub` 分开，一个是要播的，一个是收到的）；URL 直投保留为第二来源，条目标注来源，UI 必须显式提示它依赖发送端保持可达。

---

## 节点 1 · 队列引擎 `crates/horizon-cast/src/queue.rs`

队列在**一个** `Transport`（`player.rs` 的 `IMFMediaEngine` 抽象）之上做顺序/随机/单曲循环、自动续播、图片驻留、失败熔断。

`cargo test -p horizon-cast --lib queue`：**26 个用例通过**。关键几条：

| 用例 | 证明的事 |
| --- | --- |
| `the_monitor_thread_advances_a_photo_on_its_own` | 真的后台线程（150ms tick）自己会把图片推进到下一条，不是只在测试里手推 |
| `a_photo_advances_on_its_dwell_because_the_engine_never_ends_it` | 图片不会 `Phase::Ended`，驻留时间是唯一的推进依据；同时锁住 `set_image_dwell` 的 0.5s 下限（请求 0.02s 会被抬到 0.5s） |
| 连续失败熔断 | 解码不了的条目被逐个跳过，累计到阈值后写入「队列里 N 个条目都无法解码，已停止」并停机，不是无限重试 |
| 随机重排 | 一轮走完重新洗牌，且**不会**刚播完立刻重播同一条（`anchor` 把当前项钉在随机序列首位，越界时要重新洗牌，这里早期版本出过一个真 bug） |
| `replace_one` | DLNA 控制器塞进来的单条 URI 走这条路：清掉列表只留一条，播完即止 |

未验证：真实 `IMFMediaEngine` 与队列的配合（需要安装包起来，见节点 4 之后）。

---

## 节点 2 · 片库存取 `src-tauri/src/media_library.rs`

条目 = `{id, title, kind, file|url, bytes, addedAt, source}`，索引 `index.json` 与文件同目录。

`cargo test -p horizon-launcher --lib`（含本模块）：**41 个用例通过**，片库部分 14 条：

- `a_stored_item_survives_a_reopen_and_keeps_its_order` — 关掉再开顺序不变，队列的稳定性有落盘依据；
- `the_index_round_trips_through_disk` / `a_missing_index_is_rebuilt_from_the_folder` — 索引丢了能从目录重建，手写进目录的文件也能投；
- `an_index_written_by_hand_cannot_reach_outside_the_library`、`an_entry_is_only_ever_reachable_through_its_id` — 索引里写 `..\..\Windows\x` 也不会被读出来：`name_inside_root()` 拒绝空名、`.`/`..`、任何分隔符、`:`、非常规路径成分；
- `an_entry_whose_file_vanished_off_the_disk_is_not_listed` — 文件被外部删掉的条目不显示，不会出现点了没反应；
- `names_are_sanitized_and_deduped_before_they_touch_the_disk` — 同名两次上传得到 `照片.jpg` 与 `照片 (2).jpg`；
- `only_http_addresses_are_accepted_as_remote_items` — 远程条目只收 http/https，`file://`、`javascript:` 直接拒；
- `removing_an_entry_takes_its_file_with_it` / `removing_the_same_id_twice_reports_only_one_removal` — 删除是真删，第二次报「没有删东西」。

类型判定只有一处：`MediaKind::from_name` 委托给 `horizon_cast::queue::MediaKind::from_uri`，片库与队列不可能一个当视频一个当图片。

未验证：真实 `%APPDATA%\horizon-launcher\media` 目录的权限与 OneDrive 重定向场景。

---

## 节点 3 · 媒体 HTTP：Range/206 与流式上传

这是整条链上原本**完全不存在**的一环：仓库里没有任何按字节区间回话的服务端，而 `IMFMediaEngine` 解析「`moov` 在文件尾」的 MP4 时，第一件事就是 `Range: bytes=-N` 去读尾部；服务端用 `200` 回整个文件，它就报「源不支持」。

`src-tauri/src/media_http.rs` + `snapdrop_server.rs` 的 `hub_router`。

### 3.1 区间解析（单元）

`cargo test -p horizon-launcher --lib media_`：片库 14 + 区间/类型 12 = **25 通过**。

- 解析实现改成**半开区间**（`{start, end}`，`end` 不含）后才发现的原缺陷：包含式 `end` 无法表达空文件，`ByteRange::whole(0).len()` 会算出 1，于是空文件的 `Content-Length` 报 1、正文 0 字节，客户端挂等。现在 `whole(0).len() == 0`。
- 同一次改动里暴露的第二个真 bug：`bytes=-5000` 打在 1000 字节文件上时 `last - span + 1` 先做 `999-1000` **u64 下溢 panic**；改为 `length - span`。
- 语义按 RFC 7233：语法看不懂（`bytes=abc`、`items=`、多区间、`bytes=`）一律忽略回全文件；只有格式正确但满足不了的才 `416`；`bytes=0-0` 是第一字节而不是空；越界右端夹到文件尾；`start >= length` 才 unsatisfiable；u64 右端溢出用 `saturating_add(1)` 兜住。

### 3.2 真实套接字上的 HTTP（集成）

`snapdrop_server.rs` 里 `mod http_tests`，把路由表绑到 `127.0.0.1:0` 的真实监听上，用手写 HTTP/1.1 报文（`Connection: close`）读完整响应，**7 个用例通过**：

| 用例 | 断言 |
| --- | --- |
| `a_library_file_is_served_whole_and_by_range` | 1 MB 文件：无 Range → `200` + `Accept-Ranges: bytes` + `Content-Type: video/mp4` + 100 万字节逐字节相等；`bytes=100-199` → `206` + `Content-Range: bytes 100-199/1000000` + `Content-Length: 100` + 正文等于 `payload[100..200]`；`bytes=999900-` 与 `bytes=-50`（→ `bytes 999950-999999/1000000`）；`bytes=999990-1200000` 夹到 10 字节 |
| `head_reports_the_length_without_sending_bytes` | `HEAD` → `200`、`Content-Length: 4096`、**零字节正文** |
| `an_unsatisfiable_range_answers_with_the_real_length` | `bytes=5000-6000` → `416` + `bytes */1000` |
| `an_id_that_is_not_in_the_library_reads_nothing` | 未知 id → `404`；`/media/..%2f..%2findex.html` → `404`（这条路由不接收网络传来的路径） |
| `a_upload_beyond_the_default_body_limit_lands_in_the_library` | 3 MiB multipart（超过 axum 默认 2 MiB 上限）→ `200`，片库多出 1 条、`bytes` 与实际一致，再用 `bytes=2000000-` 读回尾部逐字节相等 —— 写入路径与读取路径对同一批字节达成一致 |
| `the_transfer_page_upload_still_lands_on_disk` | 旧 `/upload` 仍落到 `storage_dir`，且已改为流式写盘（原来 `field.bytes()` 会把整个文件读进内存） |
| `the_tail_of_a_half_gigabyte_file_arrives_without_the_head` | `set_len` 造出的 **500 MB** 文件：`bytes=-1024` → `206`、`Content-Range: bytes 524286976-524287999/524288000`、正文恰 1024 字节，且整体耗时远小于 5s —— 证明 seek 到尾部没有把前面 500 MB 读出来 |

计划里的三档体积（1 MB / 50 MB / 500 MB）在 3.1–3.2 中的覆盖情况：1 MB 与 500 MB 有专门用例；**50 MB 档没有单列用例**（区间语义与文件绝对大小无关，1 MB 用例已覆盖全部四种 Range 形态，500 MB 用例覆盖大文件 seek）。对安装包真实进程的 `curl -r` 留到节点 4 服务常驻之后统一做，届时用 `curl -r 0-1023`、`-r 1000000-1000999`、`-r -4096` 三段断言 206 与区间，并把输出贴进本文件。

### 3.3 路由与体积上限

`GET|HEAD /media/:id`、`POST /api/media/upload`（流式进片库）、`POST /upload`（流式进接收目录）。两条 POST 都 `DefaultBodyLimit::disable()`，改用 `UPLOAD_BYTE_LIMIT = 16 GiB` 的**每文件**上限（这不是策略，是防止客户端无限发的保险丝），超限或写盘失败会删掉半截文件——半截文件绝不能变成片库条目。

---

## 节点 4 · 片库/队列 API、播放面归一与服务常驻

一环扣一环的实测都打在**安装包真实进程**上，不是单元测试自问自答。节点 3 里承诺「留到节点 4 统一做」的真实进程 `curl -r` 类断言，本轮以 `D:\tmp\media-client.mjs`（46 项断言）覆盖，节点 3 遗留的 50 MB 档也一并补齐。

被验证的对象是工作区产物 `D:\codeX\meeting\target\release\horizon-launcher.exe`。此处登记一个踩过的坑：`src-tauri/target/release/horizon-launcher.exe` 是 10-23 的旧副本，`cargo build --release` 写的是工作区 `target/`。旧副本能起窗口却什么都不监听，一度让人以为服务常驻没生效——此后每条结论都注明是哪个二进制。

### 4.1 常驻与地址

冷启动日志（`[room]` 前缀，setup 钩子里的独立线程）：

```text
[room] 启动常驻服务
[room] 片库 C:\Users\戎耀会\AppData\Roaming\horizon-launcher\media
[snapdrop] listening on http://192.168.0.27:7878
[room] 文件服务常驻 http://192.168.0.27:7878
[room] DLNA 接收常驻 http://192.168.0.27:8290/dd.xml
```

- 7878（文件传送 + 片库 + 媒体 HTTP）与 8290（DMR HTTP）**开机即绑**，不再依赖面板是否打开；监听 2 s 内就绪。
- DMR 端口从随机改为固定 8290，`LOCATION` 从此稳定；防火墙规则 `HORIZON Meeting SnapDrop` 是按程序授权的，新端口无需新规则。绑定失败时 `start_dlna` 退回 `port: 0` 再试一次，并把两次失败原因都带进错误串。
- 设备名取设置里的 `deviceName`（实测 `GET /dd.xml` 回 `friendly=戎耀会`），缺省回落到计算机名。
- 三条协议入口（文件上传、DLNA SOAP、大屏 IPC `media_invoke`）共用同一个 `MediaRuntime`：一次 `lazy` 构造，`set_base` 由 hub 起来后回填，所以片库地址与实际监听地址不可能对不上。

### 4.2 46 项 wire 断言（0 失败，冷启动与新连接各跑一遍都复现）

| 分组 | 断言与实测值 |
| --- | --- |
| 入口 | `GET /` 200（13699 字节，文件传送页仍在）；`GET /api/media/library` 给出 `base=http://192.168.0.27:7878` 与片库根目录 |
| 上传 | 1 MB 图片入片库并识别为 `kind=image`；3 MB 通过（axum 旧 2 MiB 默认上限不再拦截）；**50 MB 通过，0.2 s 流式落盘**（`bytes=52428800`） |
| 区间 | 全量 `200` + `Accept-Ranges: bytes` + 1 048 576 字节逐字节一致；`bytes=1044480-` → `206 bytes 1044480-1048575/1048576`；`bytes=-16`（播放器读 `moov` 的写法）→ 末 16 字节；`bytes=100-199` → 100 字节；越界 → `416 bytes */1048576`；不可解析 → 忽略并回 `200` 全量；**50 MB 文件只取末 1 MiB，累计 0.3 s**（证明没把头读出来）；未知 id → `404 片库里没有这一项`，不回路径 |
| 队列 | `setQueue` 三条即开播（顺序=传入顺序，`current=0`，`playing=true`，uri 指向 hub）；`queueMove` 按 id 末位移首位；`queueRemove` 按槽位删除；`order/repeat/dwell` 三种模式写入并回读；`enqueue` 追加不抢当前项；`jump` 按 id 定位（已被移出的 id 明确 `400 队列里没有这一项`）；`hasNext` |
| 播放面 | `transport` 动词往返 `pause/play/volume/mute` 全部回状态；`status` 一次给出 `state/playing/current/queue` |
| 负例 | 未知 id → `400` 中文原因；未知 action → `400 未知操作：nosuchAction`；`../../windows/system32` 当作 id → `400`，不会变成文件路径；非 http(s) 的直投地址 → `400 只支持 http 或 https 地址` |
| 一致性 | 换新连接（模拟断线重连）读回同一条队列；`remove` 删片库项同时同步出队列；`addUrl` 直投保留原始地址并打 `local=false`（大屏据此标注「依赖发送端可达」） |
| DLNA | `/dd.xml` 200（1551 字节，含 AVTransport/RenderingControl/ConnectionManager）；SOAP `SetAVTransportURI`/`Play` → 200；控制器的一条 cast 落成一条队列（**与大屏读的是同一个状态面**）；`GetTransportInfo` 回 `TRANSITIONING`；未知动作 → 500 + `<faultstring>UPnPError` + `<errorCode>401`；`SetVolume 40`→`GetVolume 40`，且 hub 侧 `state.volume=0.4`；越界音量 → `402`；`GetProtocolInfo` 报出可播放协议 |
| 发现 | `M-SEARCH ST: ssdp:all` 从临时端口发出，收到 6 条 `200 OK`，`LOCATION: http://192.168.0.27:8290/dd.xml`，`USN`/`BOOTID` 齐；单独探针把 6 个 `NT`（rootdevice / MediaRenderer:1 / 三个服务 / uuid）全部打出来 |
| 真实解码 | 自造两段 16-bit PCM wav：入队即开播并给出真实时长（`phase=playing duration=2`）→ 第一首结束后**队列自己切到第二首**（`current=1 playing=true`）→ 第二首放完干净收尾（`playing=false`、`halted=null`、`phase=stopped`）；配对正例：整队都是坏字节时 `halted=队列里 1 个条目都无法解码，已停止`（`phase=error error=media engine error 4`）。「收尾不是故障」这条负例由这条正例锚住，两者都在真机上验过 |

自动续播的前提条件也显式断言了（`切回顺序播放 + 不循环`），因为前序用例把 `order/repeat` 留在 `shuffle/all`，那种状态下「切到第二首」证明不了顺序推进。

### 4.3 本轮改掉的真 bug 与设计调整

- `MediaEngineTransport::set_volume/set_mute`（`crates/horizon-cast/src/player.rs`）只往播放器线程**投递**命令就返回，`state()` 读到的还是旧值——wire 上表现为 `volume` 往返不回。补 `record()`：调用方线程立刻把夹到 `0.0..=1.0` 的同款值写进共享状态，播放器线程随后写同样的值，二者不矛盾。这不是「让测试通过」，投递语义本来就不保证读回。
- 解码器起不来不再拖垮整条链：`MediaStage::open` 改为必然成功，失败时挂 `Unavailable` 传输层（`load` 报 `Phase::Error` 带原因，`pause/stop` 仍可接受）。文件传送、片库 API、队列状态页都不该因为一个解码器初始化失败而整体消失。
- `deviceName` 贯通到 DLNA 公告；`dlna_start` 不再自己解析视频父窗口，父窗口只在 `media_runtime` 构造时决定一次。
- 死代码清理：删掉 `MediaStage::library()`（只有测试用），`MediaRuntime::with_transport` 标 `#[cfg(test)]`。
- 片库根可用 `HORIZON_MEDIA_LIBRARY` 覆盖：本节点套件会上传 50 MB 测试字节并造出「现场照片.jpg / 开场片.mp4」这类条目，不该落进操作员真正拿来投屏的目录。此前落进真实目录的 18 条测试条目，已用产品自己的 `remove` 接口逐条清空（`removed=18 剩余条目=0 队列=0`，目录只剩 `index.json`）——顺带也在真实目录上验了一次删除同步。

### 4.4 本节点没做到的

- 第三方 DLNA 控制器（手机投屏菜单、Windows 播放器）未验证：本机没有第二台 PC，上面所有 SOAP/SSDP 断言都来自我们自己的客户端与 loopback。渲染器说的是同一种线上格式，但那是论据，不是测量。留到节点 5。
- 坏字节媒体只能走到 `loading`/`error`，`TRANSITIONING` 也是垃圾负载下的正常读数，不能当成真实编解码路径的证据——真实证据是上面那两段 wav。
- 就绪探测踩过两个坑，记下来免得重犯：`netstat | grep :7878` 会被上一轮遗留的 `TIME_WAIT` 行骗过（必须匹配 `LISTENING`）；Git Bash 里 `kill $!` 杀不掉 GUI 子进程，要用 `taskkill //IM`。

---

## 节点 5 · DLNA 标准动词落进队列、真实 DIDL-Lite、GENA 事件、多网卡组播

节点 4 登记的四条存量问题里，属于本节点的两条（`SetNextAVTransportURI` 不落状态、`Next/Previous` 的 `701` 占位分支）已打掉，另外发现并修掉了两个只有真机会暴露的 gap。套件从 46 项扩到 **58 项，0 失败**（`D:\tmp\media-client.mjs`，对被重新编译过的真实进程跑）。

### 5.1 三个 gap，都是 wire 先找到、代码后改

1. **`Playlist::has_next()` 不看预排项**。`SetNextAVTransportURI` 成功返回后紧接着发 `Next`，控制器收到 `701 Transition not available`——它会把这读成「预排失败了」。队列在队尾确实没有后续，但 `step` 是先播放 `next_override` 再看序列的，所以「有下一条」必须把预排算进去（`crates/horizon-cast/src/queue.rs:609`）。配对断言：未预排 `Actions=Play,Stop,Seek`，预排后 `Play,Stop,Seek,Next`，越过队尾仍 `500 + <errorCode>701`。
2. **控制器投回来的本 hub 地址丢了片库身份**。DLNA 只会说一个 URI，而 `SetAVTransportURI` 拿到的正是 hub 自己印出来的 `http://192.168.0.27:7878/media/<12位id>`；走 `Track::from_uri` 后标题成了那串哈希、类别一律猜成 video。后果在真机上是硬的：still image 没有 ending 可报，被当视频的条目会让队列一直等下去，`imageDwell` 根本不生效。修法是让 `QueueDrive` 共享 `library` 与 `base`，`track_for(uri)` 先严格匹配 `{base}/media/` 前缀（再按 `? # /` 截断）回查片库；**只有本 hub 的地址才回查**——手机自己服务器上也有 `/media/xxx.mp4`，那不是我们的条目，只能保留地址尾巴能说明的部分。单元测试把这条钉住：同一条地址 `Track::from_uri(...).kind == Video`（这就是必须回查的理由），回查后是 `Image`、标题 `现场照片.jpg`，大屏 `queue` 读到的 `items[0].kind` 也是 `image`。
3. **SSDP 只在默认路由那一张网卡上加了组**（见 5.2）。

### 5.2 组播无应答：先归因，再动代码

现象是 `M-SEARCH` 一条应答都收不到，而同一份代码在节点 4 曾稳定回 6 条——所以不能直接判定「环境不行」，也不能直接改代码。逐步只动一个变量：

| 步骤 | 探针 | 读数 | 排除/确认 |
| --- | --- | --- | --- |
| a | `ssdp-watch.cjs`（bind 1900 + join 192.168.0.27，随应用重启观察） | 收到 6 条 `ssdp:alive NOTIFY`，`LOCATION` 指向 8290 | 应用侧 socket 绑上、入了组、发得出；问题在**进来的路** |
| b | `ssdp-paths.cjs`（三种到达路径） | 组播 `239.255.255.250` 无应答；单播 `192.168.0.27:1900`、`127.0.0.1:1900` 各得 `200 OK` | 渲染器在听、能收单播；不是 SOAP/端口问题 |
| c | `ssdp-iface.cjs`（发送侧 `setMulticastInterface` 逐网卡） | `192.168.0.27` → 6 条；`192.168.137.1`（ICS/热点）→ 0 条；`172.19.224.1`（vEthernet Default Switch）→ 0 条 | 两件事同时成立 |

读数 c 说明**本机的默认路由是一张虚拟网卡**：同机模拟控制器时，不指定出接口的组播压根没上线到局域网那一侧——这是发送侧事实，不是渲染器缺陷。但同一读数也确认了真正的产品缺陷：`multicast_socket()` 只 join 默认路由那张网卡，会议室里双网卡的 PC（扩展坞以太网 + 给手机用的热点）在另一张线上不可见。

修复（`crates/horizon-cast/src/discovery.rs`）：新增 `interface_addresses()`，用 `GetAdaptersInfo` 枚举本机全部 IPv4 地址（跳过 loopback 型适配器、去重、默认路由的地址排最前），`multicast_socket()` 在每个地址上做一次 `IP_ADD_MEMBERSHIP`——**只有主地址的 join 失败才让 socket 失败**，一块网卡在枚举与入组之间掉线不值得让发现整体消失。`Win32_NetworkManagement_IpHelper` 是同一个 `windows` crate 的特性开关，没有引入新依赖。

复测（重新 `cargo build --release` 后的真实进程）：主网卡 6 条应答，其余每张「组播确实上了线」的网卡各 6 条——实测 `192.168.137.1(本地连接* 10)=6条 172.19.224.1(vEthernet (Default Switch))=6条`，修复前是 0 条。断言写成「先用 joined 监听 socket 证明报文上线，上线了才要求应答」，上不了线的网卡不计成败，换机器不会出现假失败。

### 5.3 58 项 wire 断言分组（本节点新增/改写的部分）

| 分组 | 断言与实测值 |
| --- | --- |
| 标准动词 | `SetNextAVTransportURI` → 200 且**不提前进列表**（`n=1`）；`Next` → 200，预排那条落成第 2 项并成为当前（`n=2 current=1`）；越过队尾 → `500 + 701`；`GetCurrentTransportActions` 未预排 `Play,Stop,Seek` / 预排后 `Play,Stop,Seek,Next`；`Previous` 不谎报（见 5.5） |
| DIDL-Lite | `GetPositionInfo` 的 `TrackMetaData` 是转义了两层的真 DIDL（信封里 `&lt;DIDL-Lite`，解一层后带 `xmlns:dc`、`item id/parentID/restricted`）；本 hub 地址回查片库 → `<dc:title>提示音二.wav</dc:title>` + `object.item.audioItem.musicTrack`；走下来后当前项同样是片库标题与音乐类别，不是地址尾巴的哈希 |
| 外来地址 | `http://192.168.0.9:8000/stream/外部片段.mp4` 不被片库解析：DIDL 用地址尾巴当标题、`videoItem`、`protocolInfo="http-get:*:video/mp4:*"`，且队列里同一条 `local=false`（大屏据此标注「依赖投送端可达」）——与上一行构成配对，证明回查是按 base 严格限域的 |
| GENA | `SUBSCRIBE /evt/AVTransport` + `CALLBACK` 指向真实 HTTP sink → `200` + `SID: uuid:…` + 立刻收到含 `TransportState` 的初始 `LastChange`；控制器改状态后 sink 收到新 NOTIFY（`before=2 now=3 seq=2`）；陌生 SID 续订 → `412`；`UNSUBSCRIBE` 后同一 SID → `412`。事件键里加入了 `has_next`，所以预排/走位都会触发一次 `LastChange` |
| SSDP | 见 5.2 的复测读数 |
| 真实解码回归 | 顺序 + 不循环前提出发：第一首结束自动切第二首（`current=1 playing=true`）→ 第二首放完干净收尾（`playing=false halted=null`）；整队坏字节 → `halted=队列里 1 个条目都无法解码，已停止`（正例锚住负例） |

其余 40 项是节点 3/4 的上传、区间、队列、SOAP 负例，本轮全绿：说明 id 归一与入组改动没有碰坏已经在跑的东西。

### 5.4 单元侧

`cargo test --workspace`：**horizon-cast 212 通过 / 0 失败，launcher 51 通过 / 0 失败**。本节点新增 5 条测试并扩展 1 条：`dlna::tests::a_controller_stages_a_next_item_and_steps_through_the_list`、`dlna::tests::track_metadata_names_the_class_and_only_mimes_what_the_address_proves`、`media_stage::tests::a_controllers_step_and_staged_next_move_the_same_queue_the_screen_reads`、`media_stage::tests::an_address_of_ours_is_read_back_as_the_library_entry_it_points_at`、`discovery::tests::every_wire_this_machine_can_receive_on_is_listed`（Windows 上跑真实适配器枚举），以及在「队尾没有下一条」里补的预排算后续。

### 5.5 本节点没做到的 / 刻意不做的

- **第三方控制器仍未验证**：本机没有第二台 PC。上面的 SOAP、DIDL、GENA 断言全部来自我们自己的客户端与自己起的 HTTP subscriber sink；Windows  Media Player、手机投屏菜单、BubbleUPnP 一个都没跑过。线上格式相同是论据，不是测量。这条从节点 4 平移过来，仍然挂着。
- **`protocolInfo` 的 MIME 对本 hub 片库项一律 `*`**：地址里没有扩展名，而片库知道的 `audio/wav` 没有随 `Track` 传到 DLNA 层。刻意不猜——填错 MIME 会让接收端按错误格式起解码器，`*` 是 UPnP 允许的写法。要改就把 `MediaItem.source` 挂到 `Track` 上，留到有真实控制器因此拒播的时候。
- **`Previous` 不进 `GetCurrentTransportActions`**：队列能回退，但回退到第几项取决于顺序/随机模式，控制器无法预知；宁可少报一个动作，也不报一个它没法核对的动作。
- 同机组播的可达性依赖具体网卡：本机 Default Switch / ICS 两张能上线，别的机器不保证；因此 SSDP 断言不写死网卡数量，只写「上线的必须应答」。
- **环境坑（登记以免重犯）**：D: 一度 100% 满，`cargo` 写 `horizon_launcher_lib.lib` 直接 `os error 112`，同时 `netstat` 里 1900 只剩 `SSDPSRV`——磁盘满会让构建产物停留在旧版本，看起来像网络或功能问题。清掉 `target/debug/incremental`（6.7 GB）后恢复。此后构建结论一律看退出码，不信「上次能跑」。

---

## 节点 6 · 大屏自己的片库/队列面板

被测二进制：`D:\codeX\meeting\target\release\horizon-launcher.exe`（本轮重新 `npx tauri build`，PID 46564），启动带 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333`、`HORIZON_MEDIA_LIBRARY=D:\tmp\media-lib-test`。片库里 5 个条目是**真素材**（ffmpeg 生成的 1919×1032 截图、91055 B 的 JPEG、2 MB H.264、两段 3 s wav），且一律走产品自己的 `POST /api/media/upload` 入库，不是手工塞进目录的。

### 6.0 本节点的实现变更（先写清楚测的是哪一版）

- **删掉第二道 DLNA 门**：`dlna_control` / `dlna_present` 两个 `#[tauri::command]` 连同 `DmrControl` 一起移除，控制统一走 `media_invoke`。这道门比 dispatcher 弱（没有 mute），而且播放面的 DPI 缩放不对称正是它造成的。`dlna_start`/`dlna_stop` 保留——那是开关，不是控制。
- **`local` 改按主机前缀判定**：`{base}/media/` 开头才算本地，不再看路径形状；新增 `only_an_address_under_our_own_hub_counts_as_local`。
- **`/info` 报出自己的片库端口**：`BrowserMirrorConfig.hub_port`，`hub_port(base)` 从 hub 基准地址里取端口。没有做「回落 7878」：片库服务没起来时 `/info` 就少这个字段，扩展明确报「该接收端没有开启片库服务」，比悄悄打一个可能不存在的端口诚实。新增 `a_sender_learns_the_hub_port_from_the_hubs_own_address`（含 `""`、无端口、缺 scheme 三种负例）。
- **`POST /api/media/upload` 补 `OPTIONS` 预检**（新增 `the_upload_route_answers_a_preflight`：`204` + `Access-Control-Allow-Origin: *` + `Allow-Methods` 含 `POST`）。
- **IPC 门也开始记一行**：`media_invoke` 与 HTTP 门同样输出 `[ipc] <action> ok in …`。没有这一行，大屏自己的流量从外部完全不可见（见 6.4 为什么页面里数不出来）。
- **大屏 UI**：新增 `src/lib/media.ts`（类型化的 IPC 门，动作名与 HTTP 完全一致）、`src/components/MediaRail.tsx`（投屏队列 / 主机片库双页 + `useMediaViews`），重写 `DlnaStage.tsx`：数字与 `kind` 一律读队列视图，控制走 `media.*`，播放面走 `media.present`，**`Math.random()` 假频谱删除**。

### 6.1 大屏 DOM 实读（`node .probe/cast-check.mjs rail`，CDP 9333 读 DOM，不点屏幕坐标）

| 断言 | 结果 | 实测值 |
| --- | --- | --- |
| 两个页签 | 通过 | `投屏队列5 \| 主机片库5` |
| 队列行数 | 通过 | 现场照片.jpg, 开场片.mp4, 提示音一.wav, 界面截图.png, 提示音二.wav |
| 恰有一行是当前项 | 通过 | `index 0` |
| 底部计数与列表一致 | 通过 | `队列 1 / 5` |
| 没有假频谱 | 通过 | `.dlna-music__bar` 计数 **0** |
| ↑ 真的把第二行移上来 | 通过 | `现场照片,开场片,… -> 开场片,现场照片,…` |
| ↓ 放回去 | 通过 | 顺序与操作前逐项相等 |
| 循环方式改得动、读得回 | 通过 | 下发 `one` → 1.3 s 后 select 回读 `one` |
| 循环方式能回到 off | 通过 | 回读 `off` |
| 图片停留时间往返 | 通过 | 回读 `4` |
| 片库页列出条目 | 通过 | 提示音二.wav, 界面截图.png, 提示音一.wav, 开场片.mp4, 现场照片.jpg |
| 缩略图真的从 hub 画出来 | 通过 | `1919x1032  http://192.168.0.27:7878/media/02a505d4cb90`（`naturalWidth>0`） |
| 非 hub 项带标注 | 通过 | 此轮全部本地 → `all items are local`；外链的正例见 6.5 |

顺序编辑用的是行内 ↑/↓，不用拖拽：拖拽对键盘和 372 px 宽的扩展都不适用，而 `media.queueMove(id, to)` 已经是现成的动作。`queue.rs:519` 的 `reorder` 是 remove-then-insert 且同步 `current`，下标语义没动。

### 6.2 播放面矩形（`node .probe/surface-rect-check.mjs 46564`）

DOM 的 CSS rect 走 CDP，原生子窗口 `HorizonCastPlayer` 的物理 rect 走 `.probe/rects.ps1`（Win32 `EnumChildWindows`），同一台机器同一时钟；`devicePixelRatio = 1`，所以期望物理矩形就是 CSS 矩形加 `WRY_WEBVIEW` 的原点偏移。

| 场景 | 侧栏 | DOM 请求的 CSS 矩形 | 原生面实际矩形 | 宽高比 | 按比例居中 |
| --- | --- | --- | --- | --- | --- |
| 第一轮 | 开 | `0,67 1579×873` | `14,67 1552×873` | 1.7778 | 是 |
| 切一次侧栏 | 合 | `0,67 1919×873` | `184,67 1552×873` | 1.7778 | 是 |
| 再切回来 | 开 | `0,67 1579×873` | `14,67 1552×873` | 1.7778 | 是 |

差值不该是 0：原生面把 16:9 的内容**按比例放进**请求的矩形里再居中。`873 × 16 / 9 = 1552`，`round((1579−1552)/2) = 14`，`round((1919−1552)/2) = 184`——实测的 left 恰等于算出来的居中偏移，且开合侧栏后跟着新布局走。这是 `media.present` 里那处缩放修复的实测，而不是「看起来对」。

### 6.3 队列是轮询、片库是事件（`node .probe/poll-period.mjs`）

| 场景 | 外部改动反映到大屏 DOM 的延迟 |
| --- | --- |
| 舞台开 · 侧栏开 · 队列页，HTTP `enqueue` → 舞台计数 | **118 ms**（第 2 次读中） |
| 舞台开 · 侧栏开 · 片库页，同上 | **1001 ms** |
| 舞台开 · 侧栏合，同上 | **1009 ms** |
| 上传落库 → 片库页计数变化（`media-library` 事件） | **53 ms / 58 ms**（两轮） |

每轮都先把 DOM 读回基线再计时（上一版没等，第三轮 16 ms「命中」读到的是上一轮残留的旧文本——那是探针自己的错，不是产品的快）。延迟上界贴着 1 s 就是轮询周期；事件路径快了约 20 倍，两条路径由此分得开：**队列 1 s 轮询，片库读一次 + 事件更新**。

### 6.4 面板的 IPC 成本（`node .probe/ipc-log-rate.mjs D:/tmp/launcher-node6.log`）

按宿主日志 `[ipc]` 行、6 s 一个窗口计数，每轮都把面板状态打出来核对：

| 场景（状态实测打印） | 6 秒内 IPC | 每秒 | 按动作 |
| --- | --- | --- | --- |
| 舞台开 · 侧栏开 · 队列页 `{"rail":true,"tab":"投屏队列5"}` | 6 | 1.00 | `queue:6` |
| 舞台开 · 侧栏开 · 片库页 `{"rail":true,"tab":"主机片库5"}` | 6 | 1.00 | `queue:6`（**没有 library**） |
| 舞台开 · 侧栏合 `{"rail":false}` | 6 | 1.00 | `queue:6` |
| 舞台开 · 再开回来（可重复性对照） | 6 | 1.00 | `queue:6` |
| 舞台合（点了 `.dlna-stage__exit`，`.dlna-stage` 消失） | **0** | 0.00 | `{}` |

计划里估的「片库页开着 ≈3 次/秒」是错的，实测更好：**片库根本不轮询**，切页签不产生任何 `library` 调用（列表在挂载时读一次，之后靠 `media-library` 事件）。也要更正 A1-3 的另一半：队列轮询挂在**舞台**上（`MediaRail.tsx:432` 的条件是 `open`），不是挂在侧栏上，所以合上侧栏省下的是片库那次读取而不是队列轮询——1 次/秒，不是 0。

**为什么不能用页面里的计数器**：`window.chrome.webview.postMessage` 替换上不生效（宿主缓存了原生函数），`window.__TAURI_INTERNALS__` 连同它的 `invoke`/`ipc`/`postMessage` 全是 `writable: false` + `configurable: false`（`Object.defineProperty` 直接抛 `TypeError: Cannot redefine property: invoke`），应答也不走 `message` 事件（4 s 内 0 个）。所以三种钩子都实测为 0，而面板明明在轮询——上一版 `cast-check ipc-rate` 报「0 次/秒」就是这个静默失败。改成宿主日志计数之后，数字与 6.3 的轮询延迟互相印证。

### 6.5 控制器这一侧投上来的东西（`node tools/dlna-cast.mjs`）

| 步骤 | 实测 |
| --- | --- |
| 先点 `.dlna-stage__exit` 关掉舞台 | `.dlna-stage` = false |
| SOAP `SetAVTransportURI` 指向 `http://192.168.0.27:7878/media/02a505d4cb90` + `Play` → 200 | 舞台**自己开起来**（无人点击），行 = `界面截图.png [无徽标]` |
| 再投一个外来地址 `https://example.com/素材/远程片段.mp4` | 行 = `远程片段.mp4 [外链]`，标题取地址尾巴 |
| 中途 `rail-live` 读数 | 控制器改变的状态在大屏计数里看得见（`队列 1 / 1`、「第 1 条 · 共 1 条」） |

`CastPanel` 里的 `DlnaCard`（`CastPanel.tsx:1045` 起）保持**只读状态卡**：它显示设备名、`dd.xml` 地址与当前状态，不提供播放控制，控制面只有这个舞台。文档里写明，避免以后有人以为少做了一半。

### 6.6 `jump` 的自动续播语义（读代码 + 两条实测）

`crates/horizon-cast/src/queue.rs:618` 的 `Playlist::jump` 用 `let autoplay = shared.playing`——队列原本停着时，jump 只装载不起播。这是设计，不是缺陷：上一版播放面探针卡在 `相位=stopped`（连续 7 次读到 `pos=0.067 dur=6 err=null playing=false`，而显式 `transport play` 后立刻 `0.349 → 2.667`），根因就是当时当前项是图片、队列并未在播。正例：`setQueue(autoplay=true)` 让队列在播 → `jump` 到视频 → `playing=true`、`phase loading→playing`，探针按「jump 之后补一次 play，等价于操作者点播放键」写成两条路径。

### 6.7 单元、类型与线协议

`cargo test -p horizon-cast --lib` **213 通过 / 0 失败**（6 ignored），`cargo test -p horizon-launcher --lib` **54 通过 / 0 失败**，`npx tsc --noEmit` 0 错误。`HUB=http://127.0.0.1:7878 DMR=http://127.0.0.1:8290 node D:/tmp/media-client.mjs`：**合计 60 项，失败 0 项，传输层重试 0 次**（本节点新增的正/负一对：本机 hub 地址 `local=true`、路径里带 `/media/` 的外部地址 `local=false`；此前 `local` 的正例在 HTTP 上从没测过）。

### 6.8 本节点没做到的 / 环境坑（登记以免重犯）

- **扩展与安卓没动**（节点 7/8）；本节点只让大屏自己能用这套片库与队列。
- **第三方 DLNA 控制器仍然没跑过**（本机没有第二台 PC），6.5 的控制器是我们自己的 SOAP 客户端。
- **一次实例无故退出**：上一个实例（PID 39540）在探针跑完之后整机进程消失，`Application Error` 事件日志与崩溃转储里都没有记录（`Get-WinEvent` 查 40 分钟无匹配），退出前最后一条日志是正常的媒体读取。之后在新二进制上重放同序列（视频在播 + 反复开合侧栏 + 逐条断言）两轮未复现。**原因未查明**，登记在此；如果后续再出现，先查 `present`/原生子窗口这条路径。
- **假素材会让真断言假失败**：`D:\tmp\media-lib-test` 里原来的 `现场照片.jpg` 是合成字节（`07 26 45 64` 开头），缩略图断言读到 `0x0` 就报失败。改成 ffmpeg 真图并经产品自己的上传入库后，同一断言给出 `1919x1032`。
- **探针自己制造的两次假象**：(a) `cast-check rail` 曾把片库页的 ▶（只投这一条）/ ＋（追加）当成 ↑/↓ 点，队列从 4 条变成 2 条——产品严格按按钮语义执行，错在探针没先切回队列页，现在探针强制回队列页、并对拿不到的控件显式记失败而不是抛 `Illegal invocation`（那一轮整份报告被吞成 `{}`）。(b) `ipc-rate` 的 0 次/秒见 6.4。
- **undici 与 DMR 的传输层竞态**仍然存在：DMR 每个连接只答一次就 `CONNECTION: close`，node 的 fetch 池约 1–2 次/200–300 请求出现 `UND_ERR_SOCKET other side closed`，同一压力换 `node:http` 是 0/300。套件里用 `fetchNet` 重试并**把重试次数打进结论**（本轮 0 次）。这是客户端取用方式的错，不是产品行为，两条断言各测什么在 `D:/tmp/media-client.mjs` 里写明。
- 真实 `%APPDATA%\horizon-launcher\media` 的权限与 OneDrive 重定向场景仍未覆盖（本轮依旧用 `HORIZON_MEDIA_LIBRARY` 指向独立目录）。

---

## 节点 7 · 浏览器扩展：文件 + 片库 + 队列

被测对象是 `extension/` 下的**出厂脚本**（`popup.html` / `popup.js` / `media.js` / `discovery.js` / `background.js`），没有一个字节为测试改过。外壳是 `.probe/ext-stub-harness.mjs`：它只把浏览器侧能力换成桩（`chrome.storage`、两个上下文之间的消息总线、offscreen/tabs/tabCapture 的空实现、网卡列表），其余在 headless Chrome 156 里按原顺序加载并在真实网络上跑，页面发在接收端所在的局域网地址上（`http://192.168.0.27:8123/popup.html`，CDP 9445）。这台机器上 `--load-extension` 被忽略（节点 3 已登记），所以这是唯一能跑「出厂脚本 + 真实网络」的路，不是退而求其次的选项。

一个弹窗三个分页：**投屏**（原有）、**文件**（多选 → `POST /upload` → 接收目录）、**片库**（队列 + 主机片库 + 播放面控制，`popup.html:1272` 起）。片库分页里的每个按钮都映射到 `POST /api/media/<action>`，与大屏 IPC、DLNA 是**同一个 dispatcher**；没有哪个控件只在这个弹窗里成立。hub 端口不写死：扫频时 `/info` 的 `hub` 跟着接收端一起进 `chrome.storage.local`（`discovery.js:97`），只有手填地址才回 `/info` 现问一次（`media.js` 的 `resolveBase`，问不到就报「该接收端没有开启片库服务」，不猜 7878）。

### 7.1 本轮改掉的六个缺陷（四个在产品里，两个在验证装置里）

| # | 症状 | 根因 | 修法 |
| --- | --- | --- | --- |
| 1 | 扫频结束后约 10 s 内，页面连 `127.0.0.1` 都打不开 | 被 abort 的 `fetch` 不取消底层 TCP connect，48 并发把 Chrome 的连接池占满；接收端与网络无关 | `SWEEP_CONCURRENCY` 48 → 16（`discovery.js:18`，三次测量见 7.5） |
| 2 | 「扫完」以最后一个探针返回为准，但此刻选中的接收端还答不出；操作员扫完的下一个动作就是点投放 | 恢复期在探针停止后才开始，而名单在探针停止时就落盘了 | 大扫频（`total > 32`）结束时用 `reachable()` 再问选中接收端，最多 12 次、间隔 500 ms（`discovery.js:147`、`discovery.js:203`） |
| 3 | 恢复期内重开弹窗会立刻再扫一次，结果只剩 `127.0.0.1`，那台刚答过的接收端从名单里消失 | 第二轮的每个探针都在 700 ms 预算里 abort，只有 loopback 来得及回 | worker 侧 12 s 冷却（`discovery.js:162`）＋ 弹窗自动打开时 12 s 内复用已完成的名单（`popup.js:145`）；「搜索局域网」是唯一总是重扫的入口 |
| 4 | 片库条目交给播放器的地址只有 `/media/<id>`，地址里没有任何能说明「这是什么」的东西 | 只有地址可读的客户端（MediaEngine、DLNA `res`）按最后一段判类别；图片被当视频解，回报 `SRC_NOT_SUPPORTED`（`player.rs:162` 记录了这个读数的含义） | 文件名回到路径尾部 `/media/<id>/<name>`（`media_library.rs:286`、`snapdrop_server.rs:649`）；**查找仍只用 id**，名字错了只错标题不错字节；无名路由保留，缓存了旧地址的控制器不受影响 |
| 5 | 桩 harness 把接收端所在 PC 的全部网卡都交给「浏览器」：同一台接收端被发现三次（LAN、手机热点、loopback），有时自动选中手机热点地址 | 桩把浏览器当成了接收端的第二个视图，而不是局域网上的另一台设备 | 只广告 `--host` 所在 /24（`ext-stub-harness.mjs:94`）；名单回到 2 条：局域网上那台 + loopback |
| 6 | 桩 harness 依赖片库里现成的条目：片库一空，整段 media 断言报 `libraryLoaded:false` | 断言建在「别人留下的数据」上，不是自给自足 | 分页打开后先用产品自己的 `POST /api/media/upload` 传两条 `分页验证-*`（12 s wav + 70 B png），按标题找行、点按钮，收尾按标题删；`seedUpload.uploadRequests:1`、`cleanup.fixturesLeft:0` |

第 2、3 条是同一个恢复期的两半：第 2 条让「扫完」等到达，第 3 条不让下一轮再踩进去。`popup.js:152` 的短路只对自动打开生效——手动重扫永远真的重扫，否则操作员按了按钮却什么也没发生。

### 7.2 三个分页各打到哪

| 分页 | 控件 | 走哪条线 | 实测（`/tmp/harness-allow2.log`） |
| --- | --- | --- | --- |
| 投屏 | 原有投放 / 停止 | WS 9407，不经 hub | 切到其它分页后 3 s 内 hub 读取 **0** 次（`pollAfterPaneClosed.readsIn3s:0`） |
| 文件 | 选择文件（multiple）→ 发送 | `POST /upload`（multipart，多个 `file` 字段） | 2 × 3.3 KB → `成功保存 2 个文件`；磁盘上恰两个文件、各 3328 B |
| 片库·队列 | ▶ / ＋ / ↑ ↓ / ⇥ / ✕ / 点行 | `setQueue` / `enqueue` / `queueMove` / `setNext` / `queueRemove` / `jump` | 见 7.3；每项都回读队列 |
| 片库·本库 | 上传到片库 / 重新读取 / 粘贴地址 / ✕ | `POST /api/media/upload`、`GET /api/media/library`、`addUrl`、`remove` | 上传后片库从 0 → 2 项，删除后回到 1 项 |
| 播放面 | ▶ ❚❚ / ■ | `transport` `play`/`pause`/`stop` | 弹窗里只读到 `phase:"playing"`；三个动词本身在节点 4 的 wire 套件里逐个往返验过，这里没有单独点击 |

片库分页**不轮询片库**：列表在挂载时读一次，之后只在本弹窗自己的动作（`addUrl`/`remove`/`rename`/上传）后重读（`popup.js:657`）。片库页开着时 3 s 内 3 次 `queue` 读取，切回投屏后 3 s 内 0 次——轮询只属于「正在看的那一页」。

三个分页各自量了内容高度：投屏 677、片库 428、文件 271。当时对照的上限写成了 800 px，`overCap800` 全 0；**这个上限是错的**——Chrome 给工具栏弹窗的上限是 800×600，投屏页 677 px 其实已经越过 600，只是桩 harness 的窗口高 940 把它盖住了，见补记三。列表在窗口内各自滚动，不靠整页滚动兜底。片库里**没有缩略图**是刻意的（`popup.js:866`）：`/media/<id>` 只能整份下载，372 px 的弹窗为了小图把整库图片拉一遍不值得——缩略图在大屏上。

### 7.3 allow 运行：逐步实测（一次全通过，逐项可复核）

`node .probe/ext-stub-harness.mjs 504282 --host 192.168.0.27:9407 --lna allow`，报告落 `/tmp/harness-allow2.log`（`summary:"pass"`）。同一序列连跑两遍（`harness-allow.log`、`harness-allow2.log`）都通过；唯一明显的差别是前一轮的 `scanSettledAfterMs` 是 20 074 ms（真的等了恢复期，见 7.5），这一轮 11 480 ms。

| 步骤 | 断言 | 实测值 |
| --- | --- | --- |
| `contextsLoaded` | 出厂脚本按序加载、三个分页都在 | `panes:["cast","file","library"]`、`interfaces:5` |
| `scanSettledAfterMs` | 扫频真正结束（含恢复等待） | **11 480 ms**（另一轮 20 074 ms，见 7.5） |
| `receivers` | 只列真接收端，局域网那台被选中 | `count:2`：`192.168.0.27:9407`（checked）+ `127.0.0.1:9407` |
| `whoFetched` | 扫频期间只有 `/info` 探针，**弹窗自己没有媒体请求** | `total:256`（`discoveryProbes:256`，`mediaRequests:[]`，`otherRequests:[]`） |
| `directProbes` | hub 端口来自 `/info`，不是写死的 | 9407 → `hub:7878`，7 ms / 51 ms |
| `cacheRepaintsFreshPopup` | 重开弹窗复用 worker 的名单，不再扫 | `scanRequests:0`、`scanLabel:"搜索局域网"`、选中 `192.168.0.27:9407` |
| `libraryPaneState` | 分页一打开只有 library + queue 两次读 | `base:"http://192.168.0.27:7878"`、`mediaRequests:[library, queue]` |
| `seedUpload` | 两条素材经产品自己的上传接口入库 | `uploadRequests:1`、`libraryTotal:2`：192 044 B wav（`音乐`）、70 B png（`图片`） |
| `queueAfterPlay` | 片库 ▶ = `setQueue(ids:[id], autoplay:true)` | `current:0`、`playing:true`、`phase:"playing"`、`local:[true]`、无「外链」徽标 |
| `enqueueSecondItem` + `orderBeforeMove` | ＋ 追加到队尾、不抢当前项 | 2 行，`current:0`，顺序 = 上传顺序 |
| `queueRowOps` | 首行 ↑ 禁用、末行 ↓ 禁用，预排/移除可用 | 与断言逐项一致 |
| `orderAfterMove` | ↓ 真的换位，且**当前项跟着换成同一条** | `[图片, 音频] → [音频, 图片]`，`current:1`、`currentTitle` 仍是图片、`playing:true` |
| `stageNext` | ⇥ 预排为下一条 | 提示 `已预排为下一条` |
| `queueAfterRemove` | ✕ 只删这一条 | 剩 1 行 |
| `pollWhileLibraryVisible` / `pollAfterPaneClosed` | 队列 1 次/秒且只在片库页 | 3 次 / 3 s，切回投屏后 0 次 / 3 s |
| `playUploadedItem` | 片库行 ▶ 发出 `setQueue` 并替换整条队列 | `issued:["…/api/media/setQueue"]`、`busyAfterClick:false` |
| `uploadedItemPlays` | 真的在解码，不是只改了状态 | `phase:"playing"`、`正在播放 · 0:04 / 0:12`、`halted:null` |
| `deleteFromLibrary` | ✕ 删片库会连带重读队列 | 发出 `remove` → `library` → `queue` 三个请求；图片从片库消失，音频还在队列里 |
| `inboxFilesOnDisk` | 文件传送落在接收目录，字节数一致 | `C:\Users\戎耀会\Documents\meeting` 恰 2 个文件、各 3328 B |
| `cleanup` | 收尾干净，且能报出还剩几条真条目 | `inboxRemoved:2`、`libraryRemoved:[1 条]`、`fixturesLeft:0`、`realItemsLeft:0` |
| `fetchTotals` | 所有媒体流量只去 hub | 21 个请求，host 仅 `192.168.0.27:7878`；探针 0 |

截图：`docs/shots/ext-stub-idle-allow.png`、`ext-stub-live-allow.png`、`ext-stub-library-allow.png`、`ext-stub-file-allow.png`、`ext-stub-file-picked-allow.png`（`--lna block` 各有一份同名 `-block`）。

### 7.4 block 运行与 LNA：跑通了，但没证到要证的东西

`--lna block`（不传 `--disable-features=LocalNetworkAccessChecks`）走完与 allow 同一套检查，**同样全部通过**：`scanSettledAfterMs:11477`、`whoFetched.total:256`、片库与文件分页行为与 allow 逐项相同。

结论只能写到这里：**本机没有复现 LNA 拦截**。原因不是扩展被豁免——桩页自己就发在 `192.168.0.27:8123` 上，Chrome 的本地网络闸门针对的是「非本地来源的页面访问本地网络」，一个已经站在局域网地址上的页面不在它的触发条件里。所以这一轮证明不了「扩展凭 `host_permissions` 被豁免」，只证明了闸门没开时行为不变。真要验豁免，得把桩页放到公网来源（或另一台机器）上，让请求在闸门后面发起。这条记进 7.9，不当作已验证。

### 7.5 扫频把页面饿死：三次测量与一个决定

装置是 `lan-fetch-probe.mjs`：页面（`http://192.168.0.27:8124/`）里跑完一次真实的 254 地址扫频，扫频一结束就对 3 个 9407 与 2 个 7878 各发一次 3 s 预算的读；同一秒在 node 侧对同样 5 个地址发同样的读作对照。**只有并发数一个变量**。

| 并发 | 扫频本体 | 扫完立刻从页面读（3 s 预算） | 同刻 node | 决定 |
| --- | --- | --- | --- | --- |
| 48 | 254 台，明显快 | 12 s 内**任何**新连接都开不出来，连 `127.0.0.1:9407` 也超时 | 24 ms | 淘汰 |
| 24 | — | 5 个目标里仍有 1 个超时 | — | 淘汰（余量太小） |
| 16 | 254 台 / 11.3 s | 3 个 9407 全部 3.0 s abort；LAN 7878 1991 ms；loopback 7878 3 ms；随后的窗口回到 54 / 14 / 51 ms（`/tmp/probe16.json`） | 9407 24 / 50 / 51 ms、7878 4 ms | 采用**并加恢复等待** |
| 8 | 明显更慢 | 全部正常 | 正常 | 不采用：省下的失败用扫频时长偿不回来 |

「扫完之后要等」也有独立的 wire 证据：`harness-allow-3.log` 的 `directProbes` 里，扫频刚结束时 `192.168.0.27:9407` 3 016 ms 超时、而 loopback 13 ms——同一时刻接收端在 node 侧 24 ms 就答了。于是「扫完」的定义改成**选中的接收端此刻答得出**（`reachable()`，12 次、每次 500 ms 间隔，单次探针预算仍是 700 ms，最坏约 14 s）。`reachable()` 的代价只在真正的大扫频上付：候选少于 32 个时（记住的那台 + loopback）一轮不到半秒，等它没有意义（`discovery.js:203`）。

这一改动的代价直接出现在报告里：`scanSettledAfterMs` 在需要等的那一轮是 **20 074 ms**（扫频 11.3 s + 恢复等待约 8.7 s），在不需要等的那两轮是 **11 480 / 11 477 ms**。操作员看到的不是空白：worker 一边扫一边把名单写进 storage，弹窗先画缓存里记住的那台。

### 7.6 探针自己制造的假象（登记以免重犯）

- **两次点击被静默吞掉**：`run()` 与 `sendFiles()` 在 `mediaBusy` 时直接 `return`（`popup.js:641`、`popup.js:944`），而行内按钮（▶ ＋ ↑ ↓ ⇥ ✕）不在 `paintMedia` 的禁用名单里，页面上与「点了没反应」完全一样。上一轮探针在 `openPane("library")` 重读期间点 ▶ 和 ✕，日志只留下「队列 1.8 s 没变」的假象。现在每次点击前 `waitFor(!mediaBusy)`，并把 `issued`（点后新发出的请求）一起打出来；hub 每个动作实测约 150 ms，等待窗口是必要的，不是掩盖慢。
- **探针页的全局名撞车**：`lan-fetch-probe.mjs` 的页面脚本里 `function probe()` 落在 global 对象上，盖掉了 `discovery.js` 同名的探针函数，`--real-sweep` 直接把数组喂给 `rank` 崩掉。改成 `probeTargets`，并在文件里注明 classic script 的这个坑。
- **curl 的中文文件名是假象**：`curl -F file=@<中文名>` 回 `{"items":[]}`，ASCII 名正常——Git Bash 把文件名按本地编码送进 multipart，服务端读到的是另一串字节。产品路径（浏览器 `FormData`）中文名一切正常（7.3 的两条素材就是中文名）。计时类测量一律用 ASCII 名，避免把这个假象记成缺陷。
- 早期断言「缓存重绘时显示『搜索中…』」本身就是错的：缓存命中时不该出现搜索状态。现在断言的是 `scanRequests:0` 与选中项，不再断言文案。

### 7.7 「扫频要 20 秒」是取舍，不是卡死

254 个地址、16 并发、每个死地址 700 ms 预算 → 扫频本体 11.3 s；需要恢复等待的那一轮再加约 8.7 s，**实测最慢 20.1 s，理论上界约 25 s**（11.3 s + 14 s）。这个上界是**故意**买的：并发 48 时扫得快，但扫完的十几秒里页面什么也做不了（7.5），而弹窗打开后的第一个动作就是读片库或点投放。真正的交互不受影响——记住的接收端在第一个进度帧就画出来了，投屏本身不依赖扫频完成，片库分页读的是 `/info` 里带回来的 hub 端口。

### 7.8 真实片库的 8 条条目丢失（如实登记）

本阶段开始时，真实片库 `%APPDATA%\horizon-launcher\media` 里还有 8 条条目；到本节点第一份运行日志（`.probe/harness-allow-3.log`，10-31 07:03）时 `libraryTotal` 已经是 **0**。现在该目录只剩 `index.json`，内容 `[]`（最后一次写入 07:22，是 harness 删除自己素材的收尾），媒体文件本身不在磁盘上，也没有回收站或备份可恢复。

已排除：当前 harness 的删除只按标题匹配 `^分页验证-`，收尾清理也只删这个前缀；节点 6 的运行用的是 `HORIZON_MEDIA_LIBRARY=D:\tmp\media-lib-test`，没有指向真实目录；产品侧没有批量清空片库的动作（`remove` 一次一条，`clearQueue` 只清队列）。

未查明：更早一版探针（自给自足改造之前）按「片库第一行」取条目做断言，删除步骤当时没有按标题限定；那一支没有日志留存，只能算**可能的**原因，不能当结论。**这次数据丢失没有找到可复现的代码路径，原因不明**。

后果与处理：本节点的 allow/block 三轮仍然打在真实目录上——要验的就是真实路径；每次写入都在日志里（`rowsBefore`、`libraryTotal`、`libraryRemoved`、`realItemsLeft`），收尾后 0 条残留。此后建议每轮先打印片库根，除刻意的一次真实路径验证外一律用 `HORIZON_MEDIA_LIBRARY` 指向独立目录；任何写真实片库的动作都当作事故登记。

### 7.9 本节点没做到的 / 刻意不做的

- **不是真工具栏弹窗**：`--load-extension` 在这台机器的 Chrome 156 上被忽略，桩化 harness 加载出厂脚本、但 `chrome.*` 是桩。它能证明逻辑与网络行为，不证明扩展在浏览器工具栏里被装载、popup 尺寸由浏览器裁切这两件事。
- **LNA 豁免仍未验证**（7.4）：block 跑通只说明本机闸门没触发，不说明扩展被豁免。
- **⇥ 预排的结果在队列 JSON 里没有可见字段**：弹窗侧只能断言提示文案「已预排为下一条」，真正的消费在 `Next`/DLNA 上（节点 5 的 701 用例正例锚住了「预排算作有下一条」）。要让弹窗看见，需要在 queue 视图里加字段，本轮不加。
- **`mediaBusy` 期间的点击被丢弃**：这是刻意的单动作闸门（不让两个动作竞争同一份片库状态），但操作员在慢网络下会感到「点了没反应」。要么按钮进入禁用态（当前按钮在 `paintMedia` 里会禁用，但行内按钮只受 `run` 的短路约束），要么排队——本轮只在探针里等，不改交互。
- **片库分页开着就一直每秒一次 `queue` 读取**：一小时约 3 600 次；对局域网 hub 是噪声级别，但没有退避。大屏是同一节奏（节点 6），两边一致。
- **第三方 DLNA 控制器仍然没跑过**（本机没有第二台 PC）；**真实 `%APPDATA%` 的 OneDrive 重定向场景仍未覆盖**——本节点在未重定向的真实目录上跑通了上传/读取/删除，重定向没测。
- 节点 7 收尾时安卓端与扩展契约文档尚未开始；两项已在后续节点 8、9 补齐（见下），这里保留当时的范围。

---

## 节点 8 · 安卓端：多选 → 片库 → 队列

被测对象是 `android/` 下本轮新增的**出厂代码**：`HubMediaClient.kt`（主机 HTTP 客户端）、`pages/MediaHub.kt`（媒体页的片库/队列区块）、`Discovery.kt` 的 `hub` 字段与 `LanDiscovery.probeHub`。逐步证据、APK 哈希与界面说明在 [`docs/verification-2026-10-30-android.md`](verification-2026-10-30-android.md) §11，这里只登记节点结论、与计划的偏差和没做到的部分。

### 8.1 与计划的偏差（都是实现时发现的硬事实）

1. **假主机不能用 `com.sun.net.httpserver`**：原计划写 JDK 自带 HttpServer，实际单元测试类路径只有 `junit4` + `org.json`（`android.jar` 里的 `org.json` 是空桩，故测试侧单独引实现），没有 `jdk.httpserver`；不为此引新依赖，改成原始 `ServerSocket` 收一个请求回一个响应。代价是多写几十行解析，收益是 multipart 断言落在**字节**上。
2. **保留 DLNA 直投作为第二来源**：原计划写「单条投送改为多选 `setQueue`」，按已确认的决策改成新增「大屏片库 · 播放队列」区块；手机起流 + DLNA `setUri` 的单条直投仍在，标题标为「DLNA 直投」，两者在界面上分开。
3. **主机端口同样来自 `/info.hub`**：`Discovery` 解析该字段，`LanDiscovery` 用报出的端口探测片库服务；只有老接收端没有 `hub` 时才退回 7878。手柄填的地址记在 `Prefs.lastHub`，下次进页静默重连。

### 8.2 计数与实测

- 全量单测 **59 项 / 0 失败 / 3 跳过**（`ReceiverLiveTest` 2 项需 `-PhorizonToken`，`HubMediaLiveTest` 1 项需 `-PhorizonHub`）；其中 `HubMediaClientTest` 14 项、`DiscoveryTest` 9 项。
- `-PhorizonHub=192.168.0.27:7878` 对真实主机跑完整流程：

```text
[live] hub 192.168.0.27:7878 library baseline=1
[live] upload items=[f28088f4c22c:audio:192044, 4dd03899e192:image:70]
[live] setQueue phase=playing position=0.0987708
[live] transport pause→play ok
[live] image phase=playing duration=0.0
[live] queueMove order=[audio, image]
[live] queueRemove then stop ok
[live] cleanup library=1 queue=0
```

两个测量修正记在这里，免得被当成产品缺陷：`transport` 的回执是滞后的（播放器在自己的线程上执行命令），暂停要看轮询到的 `phase=paused` 而不是动词回执；`state.uri` 落后一个加载，只看 `phase=playing` 会在 12 s 的 WAV 上读到上一条的 `position=34.883`，现在等待条件锚定 `uri` 含 `/media/<id>/`（`MediaQueue.uri` 也随之进入客户端契约）。

### 8.3 本节点没做到的（如实登记）

- **没有真机 UI**：`adb devices` 为空、`emulator -list-avds` 没有 AVD，媒体页新增区块只在代码与编译产物层面存在，多选器、进度条、确认框、排序按钮没有在手机上点过。真机 E2E 是「未测到」，不是「应该可以」。
- 多选器在不同 ROM 下是否一次返回多个 URI、提供方报不出大小时走缓存复制的路径，都没有真机样本；大文件与中途断网（无续传）未测；后台/息屏时的队列轮询未测。
- 联调用例**会接管大屏正在播放的队列**（`setQueue` 覆盖、结束时 `stop`），只删自己上传的 id；本轮收尾后片库回到 1 条、队列 0 条。

---

## 节点 9 · 文档与契约

- [`docs/browser-cast.md`](browser-cast.md) 从「只讲投屏」扩成三段分页的完整契约：
  - 端口表 `9407`（镜像 `/stream` + `/info`）/ `7878`（文件页、`/upload`、`/api/media/*`、`/media/<id>/<name>`）/ `8290`（DLNA DMR）；
  - `/info` 的字段与 `hub` 缺席语义（不回落 7878，扩展报「该接收端没有开启片库服务」）；
  - 路由表与 `MediaStage::invoke` 动作表（读、队列、播放、片库），位置参数接受 `index` 或 `id`；
  - `local` 的判定是「地址以本机 `{base}/media/` 开头」，不是路径里有没有 `/media/`；
  - 队列同步 = 片库页可见时 1 s 轮询；`media-library` 事件只在上传/`addUrl`/`rename`/`remove` 时发，手机与控制器改队列不发——并附节点 6 的延迟读数（118 ms–1.0 s vs 53–58 ms）；
  - 已知限制补上「7878 无鉴权」「队列无归属」「无断点续传」，故障排查补上 7878 的自查命令。
- 安卓节点的细节落在 `docs/verification-2026-10-30-android.md` §11（发布包 0.1.7 / versionCode 8，含 sha256 与逐步输出），本文件节点 8 引用它，不复制证书式数字。
- 截图：`docs/shots/dlna-stage-music.png`（CDP DOM-only 抓取，19:03）与 `docs/shots/dlna-stage-video.png`（前台整屏实抓，19:10）在节点 6 收尾时更新；没有拿 DOM-only 截图冒充整屏实拍。
- 历史测量保留：`docs/verification-2026-10-30.md:243` 关于 `dlna_control{Seek}` 的那句没有改写，节点 6 的追记已就地补上。

### 9.1 契约的现场读数（192.168.0.27，只读）

写文档时用 `curl` 对着正在跑的安装包逐个复核了要写进契约的读数，避免「按代码写的文档」：

| 断言 | 实测值 |
| --- | --- |
| `/info` 带片库端口 | `{"hub":7878,"name":"戎耀会","paired":true,"path":"/stream","port":9407,"service":"horizon-browser"}` |
| `GET /api/media/library` | `base=http://192.168.0.27:7878`；1 条 `0f14dd6df15d`「哪吒入心之途.mp4」，`kind=video`，81,609,436 B |
| `OPTIONS /api/media/upload` | `204` + `access-control-allow-origin: *` + `access-control-allow-methods: GET,POST,HEAD,OPTIONS` |
| `HEAD /media/<id>/<name>` | `200`、`content-type: video/mp4`、`accept-ranges: bytes`、`content-length: 81609436` |
| `Range: bytes=0-15` | `206`、`content-range: bytes 0-15/81609436`、正文 16 字节 |
| `<name>` 不参与查找 | 同一 id 把文件名换成 `x.mp4` 仍回同一文件——字节由 id 决定，名字只影响标题 |
| `/media/*` 不带 CORS | 上述响应没有 `access-control-allow-origin`；播放元素与 DOM 缩略图不需要读正文 |

（这台机器上片库现有条目是用户素材，81.6 MB 的 MP4 不是测试残留；本轮测试上传的两条已清理。）

本节点没做到的：扩展三张截图来自桩化 harness（页面在局域网地址上），不是浏览器工具栏里的真弹窗；第三方 DLNA 控制器依旧没有第二台 PC 可对照；文档里的「1 s 轮询」是设计契约，节点 8 的手机端轮询在真机上的节奏仍未点亮。

## 补记 · 文件分页外观重做（2026-10-31）

起因：文件分页仍是「两枚小按钮 + 灰行列表」的旧形态，和投屏页的卡片、片库页的分组不是一套语言（旧截图已被本轮覆盖）。只改 `extension/popup.html` 与 `extension/popup.js`，接口、报文与网络行为零变化。

- 投放卡：虚线大卡 + 上传图标 +「选择文件或拖到这里」；整个分页可接收拖放，拖入时卡片高亮。
- 待发送分组：条数 + 总字节；每行是扩展名标记、文件名、大小与 `✕`，超过 168 px 滚动。
- 发送键：整宽主按钮，显示「发送 N 个文件 / 共 X」；发送中禁用并显示不确定进度条——上传仍是单次 `fetch`，没有真实百分比，不伪造。
- 回执条：状态点 + 文案，成功绿、失败红。
- 顺手修掉 `✕` 删第 1 行无效的旧缺陷（`data-drop="0"` 被当成假值）；重复选择/拖入按「名称 + 大小 + 修改时间」去重。

验证（桩 harness，出厂脚本 + 真实主机 `192.168.0.27`，`--lna allow` 与 `--lna block` 各跑一次，均 `summary:"pass"`）：

| 读数 | 实测 |
| --- | --- |
| 文件分页内容高度 | 377 px（弹窗上限 600，未裁切；本行原先误写 800，见补记三） |
| 选中 2 个文件 | `发送 2 个文件 / 共 6.5 KB`；两行标题与 3.3 KB 大小逐项正确 |
| 发送后 | 回执 `成功保存 2 个文件`；接收目录 2/2 落盘；列表清空、发送键回到禁用 |
| 拖入 2 个文件 | 投放卡高亮、2 行入列、计数 `2 个 · 32 B`、发送键可用 |
| 同一批再拖一次 | 仍是 2 行，提示「2 个文件已在列表里，重复的没有再加」 |
| 逐行 `✕` 移除 | 回到 0 行 |

截图：`ext-stub-file-picked-allow.png`（已选状态）与 `ext-stub-file-allow.png`（成功回执），`-block` 各一份。收尾 `fixturesLeft:0`，片库 6 条用户素材原样；验证期间大屏队列被 harness 接管，结束后按原 id 用 `setQueue` 还原为用户的「哪吒入心之途.mp4」（`playing:false`，与验证前一致）。

没做到的：拖拽用页面内构造的 `DataTransfer` 验证事件路径，没有从资源管理器真拖一次；弹窗仍是桩 harness 里的页面（`--load-extension` 在本机被忽略），不是工具栏真弹窗；进度条只有不确定态；窄于 372 px 的窗口未测。

## 补记二 · 弹窗外框圆角（2026-10-31）

- 页面不再全出血：`body` 留 6 px 近黑外框，`.shell` 成为 20 px 圆角面板（1 px 发丝边、顶部内高光、柔和投影）；原先挂在 `body::before` 的两团环境光收进面板背景，不再溢出圆角。
- 代价：内容宽度由 340 px 收到 326 px。收窄把列表里潜伏的横向溢出顶了出来（队列/片库出现横向滚动条、行内按钮被切），已修：`.rows` 改为 `minmax(0, 1fr)` 单列、`.row` 加 `min-width: 0`。
- 实测：`--lna allow` 与 `--lna block` 各重跑一次，均 `summary:"pass"`；三页高度快照 677 / 442 / 377 px。当轮记成「仍低于 800 px 上限」，实际上限是 600 px，投屏页当时已越限（补记三更正）；新截图里队列的 4 个行内按钮与片库的 3 个按钮完整，横向滚动条消失。
- 边界：弹窗窗口本身的圆角由 Chrome/Windows 绘制，扩展只能画页面内这层；本轮改的是页面外框。截图仍来自桩 harness，不是工具栏真弹窗。

**验证期间的数据变动（如实登记）**：本轮 08:14 前后，真实片库里 5 条「屏幕截图 2026-08-26 …」从索引与磁盘同时消失（目录只剩 `index.json` 与「哪吒入心之途.mp4」）。harness 只删除 `^分页验证-` 前缀的夹具：第一遍开始/结束都是 6 条，第二遍开始时已是 1 条；这段时间本仓库没有其它写片库的脚本，原因未查明。与 7.8 同类，继续按事故登记；此后任何写真实片库的动作都要先打印根目录并留日志。

## 补记三 · 投屏分页去掉默认滚动条（2026-10-31）

起因：投屏页一打开就带一条竖向滚动条。根因不是样式写错，是高度超了：Chrome 工具栏弹窗的上限是 **800×600**（Chromium 源码 `kMaxSize = {800, 600}`），内容高于 600 px 就由弹窗自己长出滚动条。此前文档里的「800 px 上限」把宽和高看成了一个数；桩 harness 的窗口高 940，页面永远放得下，所以这条一直没被测出来。用本地量高脚本（`D:/tmp/measure-popup.mjs`，剥掉脚本只留样式与结构）量到当时投屏页 `body` 高 **707 px**（`.shell` 695 + 外层 12）。

只改 `extension/popup.html` 的样式，`popup.js`、接口与网络行为零变化：

- 接收端行从两行（名称 / 地址）收成一行：名称、地址、徽标同在一条基线上，地址过宽省略；单行高度约 52 → 34 px，这是省得最多的一处。
- 接收端列表封顶 74 px（正好两行 34×2+5），第三台起在卡片内部滚动，不再把弹窗撑高；真滚动时第三行会露出 1 px 的边，作为「下面还有」的提示。
- 滚动条统一改成 6 px 圆角深色滑块（`scrollbar-width: thin` + `::-webkit-scrollbar`）：原生滚动条约 15 px、浅灰，塞在 326 px 的深色卡片里像渲染缺陷；这一条同时作用于队列/片库/文件列表。
- 外壳/顶栏/分页签的间距 15→12、13→9、12→9；hero 内边距 14→9/12、光环 56→44、标题 15.5→14.5；接收卡外边距 14→10、内边距 12→10；投屏码输入 44→38 px（字号 20→19）；两个投放键的内边距 10/12→7/11、间距 8→6、图标 34→30；页脚外边距 12→6。
- `.rows` 那条「弹窗在 800 px 停止增长」的注释一并改成 800×600 的真实约束。

实测（本地量高脚本，注入 1 / 2 / 3 个样例接收端）：

| 状态 | `body` 高 | 上限 | 结论 |
| --- | --- | --- | --- |
| 1 台接收端 | 541 px | 600 | 富余 59 px |
| 2 台接收端 | 580 px | 600 | 富余 20 px |
| 3 台接收端 | 581 px | 600 | 列表内部滚动，弹窗不再长高 |
| 出错态（2 台 + 3 行错误说明） | 594 px | 600 | 富余 6 px；错误说明不截断 |

出错态特意量过：`hero` 的说明文字平时单行省略，出错时允许换行，三段文字会把投屏页顶到 603 px——比上限多 3 px，于是把上面那几处 1–2 px 的间距一起收掉，落到 594 px，错误全文仍然完整显示。

桩 harness 同步把量高从「`.shell` 对 800」改成「`body.scrollHeight` 对 600」，并在 allow 运行里硬断言投屏页不得超过 600（超了直接判失败），而不是记录一个永远为 0 的数。

`--lna allow` 与 `--lna block` 各重跑一次，均 `summary:"pass"`：

| 读数 | allow 实测 |
| --- | --- |
| `castPaneHeight`（2 台真接收端） | `bodyHeight:565`、`shellHeight:553`、`cap:600`、`overCap:0` |
| 片库分页 / 文件分页高度 | 443 px / 372 px，`overCap:0`（block 轮片库 437 px） |
| 上传、入队、换序、预排、移除、轮询节奏 | 与 7.3 逐项一致（`seedUpload:seeded`、`queueAfterPlay.playing:true`、`pollAfterPaneClosed:0`） |
| 文件传送与拖拽 | 2/2 落盘、去重与逐行移除同 7.3 |
| 收尾 | `fixturesLeft:0`、`realItemsLeft:1`（用户的「哪吒入心之途.mp4」原样） |

截图重新生成：`ext-stub-idle-{allow,block}.png`、`ext-stub-live-{allow,block}.png`、`ext-stub-library-{allow,block}.png`、`ext-stub-file-{allow,block}.png`、`ext-stub-file-picked-{allow,block}.png`。验证期间大屏队列被 harness 接管，结束后按原 id 用 `setQueue` 还原为 `["0f14dd6df15d"]`、`playing:false`，与验证前一致；接收目录只落了两份夹具、结束即删。

没做到的：本机 `--load-extension` 仍被忽略，所以「没有滚动条」是**量高 + Chromium 公布的 600 px 上限**推出来的，不是在真工具栏弹窗里亲眼看到的；三台以上接收端与出错态的长错误说明都只在本地量高脚本里构造，真实扫频只找到 2 台、真错误也没触发过；系统文字缩放不等于 100% 时字号会被放大，正常态 20 px 的富余够用，出错态只剩 6 px，未测；截图里名为 live 的那张其实停在 idle——桩 harness 的 `start` 只回了 `{ok:true}`，没把运行状态回给页面，这是既有现象，本轮没动它。

## 补记四 · 播放界面默认收起片库与队列（2026-10-31）

改前：`DlnaStage` 一打开就带着右侧 340 px 的「片库与队列」栏（`useState(true)`）。大屏是给人看画面的，栏一开就把 1920 宽的舞台挤成 1580，投放的第一眼看到的是一半画面加一排管理控件。

改后只有一行（`src/components/DlnaStage.tsx:91`）：

```tsx
// The stage opens on the picture: a 340px rail beside the frame shrinks the
// video the room came to watch, so it stays collapsed until asked for.
const [railOpen, setRailOpen] = useState(false);
```

开合按键本来就在底部状态条右侧（`.dlna-stage__rail-toggle`，文字「片库与队列」+ 队列条数，带 `aria-pressed`），不需要新控件。栏收起时，栏里才会出现的报错/停队列原因改由状态条上的 `.dlna-stage__warn` 显示（`railNote`），收起不等于看不到问题。

验证走真机 DOM，不看源码：`npx tauri build --no-bundle` 重新打包时旧进程 PID 16348 占着 `target\release\horizon-launcher.exe`，链接阶段报 `failed to remove file … 拒绝访问 (os error 5)`；`Stop-Process` 停掉旧进程后重建成功（3 分 59 秒），新进程 PID 21316，带 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333` 启动，经 CDP 9333 `Runtime.evaluate` 读数：

| 时刻 | `stage` | `rail` | `toggle` / `pressed` | `.dlna-stage__main` 宽 | 栏宽 |
| --- | --- | --- | --- | --- | --- |
| 从 DLNA 卡片点「全屏观看」进入 | true | false | 「片库与队列 0」/ `false` | 1919 px | 0 |
| 点一次底部「片库与队列」 | true | true | 「片库与队列 0」/ `true` | 1579 px | 340 px（页签 投屏队列 0 / 主机片库 1） |
| 再点一次 | true | false | 「片库与队列 0」/ `false` | 1919 px | 0 |

截图：`docs/shots/dlna-stage-rail-closed.png`（默认收起，画面占满）、`docs/shots/dlna-stage-rail-open.png`（展开后的 340 px 栏），两张都逐张看过。

队列是内存态，重打包即清空；验证前后都用 `POST /api/media/setQueue {"ids":["0f14dd6df15d"],"autoplay":false}` 还原成「哪吒入心之途.mp4」一条、停住，并把播放界面和投屏抽屉都关掉，屏幕回到验证前的样子。真实片库全程只读，仍是 1 条。

没做到的：这一轮没有正在播放的媒体，所以没复测原生视频子窗口在栏开合后的矩形重排（`media.present` 那段逻辑没动，开合仍会触发它，但没有实测读数）；CDP 的 `Page.captureScreenshot` 只截 DOM，原生视频面在截图里本来就是黑的，收起/展开的视觉效果靠 DOM 尺寸对比，不能代替肉眼在真机上看片。

## 补记五 · 扩展图标与安卓 App 一致（2026-10-31）

改前 `extension/manifest.json` 里没有任何图标，Chrome 工具栏、扩展管理页和扩展卡片上显示的都是默认拼图块——扩展本身没有身份。

改后：

- `manifest.json` 增加 `icons`（16/32/48/128）与 `action.default_icon`（16/32），四条路径都指向 `extension/icons/`；
- 四张 PNG 由 `src-tauri/app-icon.svg` 渲染——和安卓 `mipmap-*/ic_launcher*.png`、桌面 `icons/128x128.png` 是同一份矢量，不是重画的近似图；
- 渲染时按 `56/1024` 内缩、`204/1024` 圆角的圆角矩形做了一次遮罩：母版的行星球体画到瓷砖底边之外，不遮罩的话下面两个角是满的、上面两个角是圆的；安卓自适应图标本来就会被启动器裁成圆角方块，遮罩后与手机上看到的样子一致；
- 生成脚本 `.probe/icon-render.mjs`：无头 Chrome 里用 canvas 画 4× 再降采样到目标尺寸。这里踩到一个坑——Chrome 156 加了 `--default-background-color=00000000` 之后 `Page.captureScreenshot` 直接不返回，所以没有走截图，透明底由 canvas 的 `toDataURL` 直接给出。

对照图 `docs/shots/ext-icon-parity.png`：左起依次是 Android `ic_launcher`、扩展 `icon128`、`icon48`、`icon32`、`icon16` 放大 8 倍，以及 16 px 实大的工具栏示意。128 与安卓图标同一张画面（H、行星弧线、光环、深色圆角瓷砖）。

验证走了三处，都在浏览器自己也认这条路上：

| 检查 | 结果 |
| --- | --- |
| Edge 加载未打包扩展后的扩展记录（`chrome.developerPrivate.getExtensionsInfo`） | `name:"HORIZON 投屏"`、`version:"0.5.0"`、`state:"ENABLED"`、`manifestErrors:0`、`runtimeErrors:0`，id `afmianelenoggnpojppcdfpbbmplpmhp` |
| 在扩展自己的 `popup.html` 里按 manifest 声明的路径 `fetch` 四张图 | 四个 `200`，字节数 694 / 1870 / 3355 / 14610，解码尺寸正好 16 / 32 / 48 / 128，四角 alpha 全为 0、中心不透明 |
| `chrome --pack-extension` 打包 | 生成 59,399 B 的 `.crx`，`unzip -l` 里 `icons/icon{16,32,48,128}.png` 四条都在 |

`--load-extension` 在这台机器的 Chrome 上仍然被忽略（带扩展名录的 `chrome://extensions` 里 0 条），所以运行时那一栏是用 Edge 做的；Edge 与 Chrome 同属 Chromium，扩展清单与图标契约一致，但这也意味着"Chrome 工具栏里长什么样"没有被亲眼看到。

没做到的：无头浏览器没有工具栏，`chrome://extensions` 在无头 Edge 里渲染成空白（`readyState:complete` 但 `extensions-manager` 不存在），所以没有扩展卡片的真机截图，对照图里的"工具栏 16 px 实大"是本地拼的示意图，不是产品界面；16 px 下 H 只有约 3.6 px 高，认得出是深色瓷砖上的亮 H，但细节（星点、光环）在 16 px 已经化开，这是直接沿用母版而不是为小尺寸另画一张简化版的结果；扩展弹窗顶部那枚同心圆标记是连接状态指示（live 变绿、error 变红），不是品牌标志，本轮没有动它。

## 仍挂着的存量问题（跨节点）

- 本机没有第二台 PC，第三方 DLNA 控制器只能用软件控制器模拟，这一点在结论里必须写明；
- 这台机器上 Chrome 的 `--load-extension` 被忽略，扩展的唯一证据来源是桩化 harness（`.probe/ext-stub-harness.mjs`），它不是真工具栏弹窗；
- 真实 `%APPDATA%\horizon-launcher\media` 的 OneDrive 重定向场景仍未覆盖（未重定向的真实目录已在节点 7 跑通）；
- 真实片库 8 条条目的丢失原因未查明（7.8），任何写真实片库的动作都要当事故登记；
- 安卓端没有手机与 AVD，节点 8 的手机 UI 未点亮；LNA 豁免（7.4）仍未证到。

