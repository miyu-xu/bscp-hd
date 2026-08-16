# HD V2 架构与跨平台边界

## 进程与依赖

```text
hd.exe (winit + platform titlebar + disjoint Wry page surfaces + NativeDisplayHost)
        │ HTTP V2 + bearer / viewport heartbeat
                         │
                         ▼
     hd-host.exe ───── redb / leases / uploads / diagnostics / reconciliation
        │ authenticated WorkerProtocolV2（每实例 secret）
        ├──────── hd-worker.exe ── crosvm ── Android Guest
        ├──────── hd-worker.exe ── crosvm ── Android Guest
        └──────── ...
                         │
                         ├─ crosvm HWND becomes a child of hd.exe's private viewport HWND (Windows)
                         ├─ ADB readiness/actions/install/screencap
                         ├─ strict frame broker generation
                         └─ signed device components / Guest bridge
```

依赖方向固定：

- `hd-core`：V2 配置、协议、状态、制品、租约、诊断和 frame 数据；不依赖 OS、窗口或进程 API。
- `hd-platform`：数据目录安全、文件原子性、进程身份/containment、磁盘复制、资源/虚拟化探测和 `VmBackend` 等窄边界；unsafe 只在 Windows/Unix 模块。
- `hd-runtime`：Host/Worker、HTTP/IPC、redb、签名制品、租约、crosvm、ADB、journal、上传和诊断。
- `hd-frame`：三缓冲外部资源、显式同步、generation 和所有权验证；不存在像素缓冲回退接口。
- `hd-device-sim`：版本化且确定性的定位、电池、网络条件和固定传感器控制协议；传感器的 0 时长表示持续覆盖，有界时长由单一可唤醒监督器按传感器独立恢复默认值，同一传感器的新覆盖原子取代旧截止时间。三轴姿态使用与 AOSP Cuttlefish 相同的负角度 `Rz × Ry × Rx` 坐标模型，在同一 revision 原子提交 accelerometer、magnetometer 和 gyroscope；前两者保持最终姿态，角速度按显式转换时间独立归零。蓝牙/NFC 由正式 adapter 负责。
- `hd-adb-bridge`：Windows 上的 owner-only loopback TCP 到 crosvm host-initiated guest-vsock 命名管道桥；每条 ADB 连接先通过 VM control `connect_vsock` 绑定同一 CID/guest:5555，再进行无解析字节转发。
- `hd-casimir-adapter`：把固定 AOSP Casimir NFCC/RF 引擎嵌入正式组件，并提供受鉴权的 Type 2 内存标签与 Type 4 NDEF/APDU 标签控制。Windows 双向桥接 Guest NCI 命名管道；macOS 把 crosvm 只追加的 Guest 输出文件作为可持续增长的流读取（当前 EOF 只表示暂时无数据），并通过 owner-only FIFO 写回 Guest。Casimir TCP 重连沿用同一输出游标，不重放历史 NCI；控制 Unix socket 固定为 `0600`，活动监听者拒绝被接管，只回收已拒绝连接的陈旧 socket，退出 guard 也只删除自身绑定的 device/inode。
- `hd-rootcanal-adapter`：把固定 AOSP RootCanal `DualModeController`、LMP/LLCP Rust 引擎和平台 AES 嵌入正式组件，并提供受鉴权的 BLE 广播 peer 与最小 GAP/GATT Device Name 服务。Windows 使用 BCrypt 和 Guest H4 命名管道；macOS 使用 CommonCrypto，把 crosvm 只追加的 Guest H4 输出文件按完整 packet 增量 framing，并通过 owner-only FIFO 写回 Guest。macOS 控制 Unix socket 固定为 `0600`，活动监听者拒绝被接管，只回收陈旧 socket，退出 guard 只删除自身绑定的 device/inode。
- `hd-frame-producer`：Windows 上的严格 frame broker；按 gfxstream Vulkan 物理设备 LUID 选择同一适配器，导入三份 `VK_KHR_external_memory_win32` image memory，执行 GPU 完成 fence 到 broker release 的单调所有权协议，全程不映射像素或提供 copy/readback 回退。
- `hd-peripheral-adapters`：只有 UWB 与 modem 拥有正式 Guest 数据面、独立进程、最小 Guest 通道授权和 bearer。modem adapter 监听 Guest RIL 固定使用的 host-vsock 9697：Windows 经 crosvm AVF 命名管道帧桥接，macOS 经按 Guest CID 隔离的原始 Unix stream，并提供确定性 AT baseline；运行时状态变化按照 AOSP r14 modem simulator 主动发布 `+CREG/+CGREG/+CEREG` 与 `+CSQ` unsolicited result，运营商切换通过一次有界 deregister/register 事务触发 Guest 重新查询 `+COPS`，避免只更新 Host 状态。macOS socket 为 `0600`、拒绝活动或异用户端点接管、只安全回收陈旧端点。UWB adapter 提供 Android 15 可解析的 FiRa v2 capability、session init/config/start/stop/deinit 状态机与短地址确定性距离报告，Windows 使用 Guest 命名管道，macOS 使用只追加输出文件和 owner-only FIFO，并采用防接管的 `0600` Unix 控制 socket；它不声明物理 RF、角度或 CCC 一致性。network、audio、camera 仅保留未来 adapter profile，其 probe 必须返回 `formal=false` 且拒绝 `serve_v2`；当前 network/virtio-net、audio/virtio-snd 与 Guest virtual camera 数据面分别由 crosvm/Android provider 提供。
- `devices.audio` 建立 Guest virtio-snd 与 Host 播放端：Windows 使用 WASAPI，macOS 使用内建于 crosvm 的 CoreAudio AudioQueue 后端。`host_audio_input` 是独立的按实例隐私开关，默认 `disabled`；Windows 只有用户显式选择 `default_microphone` 并重启该实例时才创建 capture stream，关闭后通过零个 input PCM device 从启动参数层撤销采集。macOS 已实证 48 kHz/S16 双声道多缓冲 Host 播放。直接在 crosvm 内开启 AudioQueue input 的 QA 能建立 Guest capture PCM 并按时产生 WAV，但 PCM 全零且没有 TCC 授权记录；因此产品能力和 UI 继续阻断 Host 麦克风。后续采集必须迁移到具有稳定 TCC 身份的签名 Host/XPC 进程，再以受鉴权、有界的音频通道交给实例，不能把静音帧误判为真实采集。
- Windows crosvm 正式运行使用 `warn` 日志级别；逐 MMIO 的 `info` 追踪会在单次启动写入数万至十万行，既不属于默认诊断证据，也会显著拖慢 Guest 启动。需要底层追踪时应在隔离的专项复现中显式提高日志级别。
- `hd-host`、`hd-worker`：独立可执行进程。
- `hd.exe`：单进程 Rust 桌面壳，使用 winit 管理窗口、Wry/WebView2 呈现 React 多实例管理与内联设置，并用 `NativeDisplayHost` 承载 Android；只通过 `HostClientV2` 操作 Host，不持有 VM、磁盘或帧像素。macOS 根 NSWindow、原生显示层和 WebView 只在首次 `Resumed` 后从 `PendingShell` 单向初始化为 `RunningShell`，避免 event handler 安装前的原生回调；重复恢复复用同一状态，初始化失败穿出事件循环交给原生 fatal dialog。根窗口在 WebView 首屏 ready 前保持隐藏，但必须在 15 秒内聚合当前页面必需 surface 并由原生窗口 API 确认可见；可见性请求被拒绝或超时都记录结构化事件、退出事件循环并进入同一 fatal dialog，不能留下 Host 健康但界面永久不可见的幽灵进程。常规状态刷新使用单次原子 UI snapshot 和页面/焦点自适应间隔；原生 `Occluded` 与最小化共同定义窗口不可见态，将稳定 snapshot 从失焦的 2 秒进一步降至 5 秒，但不释放 DisplaySession 或 gfxstream 表面，避免恢复黑屏。会启动外部进程的 macOS 网络状态检查只在前台设备页按需执行，其他页面、后台、遮挡与最小化状态完全暂停。`hdctl` 提供无界面控制。

默认桌面 profile 在进入制品发现、Host 启动或 WebView 初始化前执行单实例激活。macOS 按 bundle identity 激活既有 NSApplication；Windows 使用当前登录会话内的命名互斥体，并用仅由默认 profile 持有的 PID marker 验证目标 Win32 根窗口，再执行 restore/foreground。这样显式 `--data-root` 的隔离 QA profile 仍可并行，默认第二次启动则只唤醒原窗口，不会生成第二个 HD、Host 或 Android-only 窗口。

macOS UI 事件循环全生命周期只能创建一次 `EventLoopProxy<UserEvent>`，并以 `Arc` 共享给 WebView IPC、原生标题栏回调和异步任务。禁止在轮询或每请求路径克隆原始 winit proxy：winit 0.30 的 macOS 后端每次 clone 都会向 CFRunLoop 注册新的 source，而销毁 clone 不会移除该 source，最终会使空闲 Player 主线程持续执行 `CFRunLoopAddSource` 并占满一个 CPU 核。共享 `Arc` 不改变跨线程唤醒语义，也不持有 Guest 或显示资源。

Windows 的实例画面由 crosvm `gpu_display_win` 持有原生输入 HWND、WndProc 与输入路径，gfxstream 负责 Guest Vulkan 渲染和 color buffer，同构建启用的 `vulkan_display` 负责导入 image/semaphore 并呈现到同级 `subWin`。crosvm 初始窗口遵守 `hidden=true`，不会先显示独立顶层窗口。`hd.exe` 在 winit 顶层窗口中先创建黑色 `NativeDisplayHost`，再创建三个相互分离的 WebView 子窗口：固定顶栏、覆盖式侧栏和非 Player 内容。Windows 不创建 Win32 标题栏按钮层；平台代码只为根窗口安装电源生命周期、单实例身份和固定内容宽高比控制。Windows 顶栏占独立、紧凑的内容高度，Android 只使用其下方区域并始终保持 Guest 宽高比；侧栏覆盖 Android 而不改变其尺寸。Player 页不创建覆盖 Android 区域的内容 WebView；设置、设备、诊断页、瞬时小布局和最小化只隐藏 HD 自有的 `NativeDisplayHost`，保留版本化显示会话、两个原生子窗口与最后呈现帧，返回 Player 时不得重建 swapchain。顶栏在 `pointerdown` 就阻止 WebView 默认获取焦点，并在 `mousedown` 再次防守；输入动作不进入全局 busy 广播，状态与布局只在值变化时发送，避免 Android 输入 HWND、WebView2 与 DWM 之间的焦点/重排中间帧。30px 独立顶栏不挂载会越过其 HWND 边界的 React Portal/浮层；按钮提示使用不会触发额外 React 合成的 `title` 与 ARIA。标题栏 11 项动作、窗口控制、录屏和 FPS 状态与 macOS AppKit 标题栏共用平台无关契约，主/副屏选择不放入标题栏。短期、instance/generation 绑定的 DisplaySessionV2 通过 Worker 将 crosvm 输入 HWND 与 gfxstream `subWin` 一并改为 `WS_CHILD`、按物理像素 resize，完成后才显示。WebView 不接触外部 Vulkan image，显示链不经过纹理复制、CPU readback 或软件 blit；普通恢复和 crosvm resume 复用当前会话，只有实例/scanout 切换、Stop/Delete/Powerwash、原生应用生命周期 suspend 或 UI 关闭才释放。UI 关闭、崩溃、心跳过期或 detach 时，Worker 把输入 HWND 与 `subWin` 同时隐藏并移入长期存活的原生 parking host；重新启动 `hd.exe` 会在不重启 Guest 的情况下同时取回二者。parking host 和所有未选 scanout 始终不可见，不能形成 Android-only 顶层窗口。截图从 Guest ADB `screencap` 保存到用户图片目录下的 `HD`，不读取 Host Vulkan surface。gfxstream 在 GPU completion future 完成后，将同一 color buffer 的 Win32 external-memory handle 交给 `hd-frame-producer` 导入并等待严格 release，再完成 virtio-gpu timeline。

触摸拖动保持单一 Linux type-B contact，并按显示刷新率合并高频 `WM_MOUSEMOVE`。HD 的窗口线程只异步投递可合并的移动事件，不等待 crosvm 的渲染线程；按下、抬起和滚轮继续使用有界同步交付。节拍器保留 Host 采样与显示周期的分数余量，不能因常见 125 Hz 鼠标与 60 Hz 显示不整除而退化为约 41 Hz。

嵌入 Player 后，crosvm HWND 保留完整 client rect、消息队列、鼠标捕获和坐标变换，但设置为空可视区域；它是输入面而不是第二个黑色画面。这样跨进程 sibling/focus 顺序短暂变化也不会把 crosvm 的不透明黑色类背景暴露在 gfxstream 之上，且不改变 gfxstream Vulkan 零拷贝 surface。

WebView 标题栏动作恢复 Android 键盘焦点时，HD 先直接查询精确 crosvm 输入线程；目标已经持有焦点时立即返回，避免重复的 input-queue/DWM 焦点事务。只有焦点确实变化时才临时附着当前 UI 线程与 crosvm 输入线程的 Win32 input queue，验证后立即解绑；裸跨线程 `SetFocus`、永久队列附着以及把焦点留在 WebView 都不符合产品契约。

Windows 原生鼠标消息由最上层 gfxstream/Vulkan 零拷贝子窗口转发到同 scanout 的 crosvm 输入 HWND，再由每显示器 `EventDevice` 写入独立 virtio multitouch 设备。内部 `StreamChannel` 使用 `PIPE_NOWAIT`；Windows 在管道暂时为空时返回 `ERROR_NO_DATA`，base 层将其规范化为 `WouldBlock`。virtio-input 工作线程必须把该结果视为可重试的空读并继续保留 WaitContext 注册，只有真实关闭/挂断才移除 descriptor，否则启动阶段的空管道唤醒会永久切断后续鼠标事件。

macOS 日常显示继续使用 gfxstream Metal surface → `CAContext` → Player `CALayerHost` 的原生路径，不做 CPU readback。一个 Android 实例可在冷启动时配置主屏和最多三个副屏；产品 UUID 在运行时确定性映射为 scanout 0–3，gfxstream HWC 显示 ID 使用 0、2、4、6。Player 只有一个原生 surface，切换时只呈现所选 scanout；每个显示有独立 virtio multitouch，输入只路由到当前显示。Android 会把主屏 boot density 默认套到所有 internal display，因此 ADB 就绪门按相同 HWC ID 写入并回读每个副屏的 `wm density -d`，使 UI 配置的 DPI 成为真实逻辑 DPI，而不只是 EDID 物理值。

屏幕录制是用户显式启动的独立生命周期：Worker 通过每个 run 唯一、owner-only 的本地端点控制 gfxstream，macOS 使用 0600 Unix socket，Windows 使用拒绝远端且 DACL 只授权当前用户的 named pipe。gfxstream 只在录制期间为所选 scanout 注册 recording readback callback，并在提交 GPU 读回前把频率限制为最高 30fps。macOS 把 BGRA 帧交给要求 VideoToolbox 硬件加速的 AVFoundation H.264/MP4 writer；Windows 交给启用硬件 MFT 的 Media Foundation Sink Writer，再通过 `IMFSinkWriterEx` 取回实际编码器，只有存在硬件设备 URL 的 H.264 MFT 才接受，软件编码链立即失败并删除产物。Windows callback 与编码线程之间只保留最新一帧，Host 编码阻塞不能形成无界队列或拖累 Player posting lock。输出文件从 writer 创建到完成封装均限制为当前用户。停止时用保留的最后一帧写入相隔一帧的终止时间戳，避免静态画面压缩用户实际录制时长，随后注销 callback、完成 `moov` 并由 Worker 复验 MP4 与 SHA-256。最长时长和编码器失败都由可唤醒且可 join 的生命周期线程收口，不允许 detached timer 在 gfxstream/FrameBuffer 析构后访问录制状态。START/STOP 响应超时或断开时，Worker 会补发有界 STOP；gfxstream 在处理 STOP 时先注销 callback 再等待 writer 封装，避免控制响应丢失后继续读回。录屏支路明确允许有界、限速的 GPU→CPU 非零拷贝读回，但 Player 同时继续呈现原有零拷贝 surface；停止或失败必须立即注销读回，不得把录屏缓存变成显示源或永久回退。该产品不恢复会创建 Android 虚拟显示并可能锁死后续旋转的 Guest `screenrecord`，也不改变非录制状态的零拷贝显示能力声明。

独立触控板是 Android 实例的可选设备，不复用显示 surface 的直接触控坐标。Worker 在启动 crosvm 前创建每个 run 唯一的 owner-only 端点：macOS 为 Unix socket，Windows 为拒绝远程客户端且仅当前用户可访问的 named pipe；crosvm 作为客户端连接并建立独立 Trackpad virtio-input 设备。侧栏触控区覆盖 Android 上方但不参与窗口宽高或 Guest 分辨率计算，UI 把坐标归一化到固定轴范围、将移动限制为最高 30Hz。UI→Host 队列固定为 64 项，接受 Down 前为最新 Move 和不可丢失的 Up 预留空间，连续 Move 原位合并；Move/Up 永远绑定 Down 时的实例，选择切换、侧栏关闭或失焦不能把释放事件改投其他实例。实例未启用、Down 时非 Ready、端点不属于当前 run 或 Microdroid 请求均必须拒绝。

完整 Android bugreport 是与 Host-only 诊断包分离的显式敏感数据产物。HTTP/CLI 只接受实例 UUID，不接受路径或 shell；Host 生成 bugreport UUID 和 diagnostics 根下的固定文件名，Worker 只对当前 Ready Android/run 调用 `adb -s <serial> bugreport <path>`。ADB 操作限制 10 分钟并在生成中监视 256 MiB 上限，IPC 仅对该 typed 命令放宽到 10 分 30 秒，客户端保持 11 分钟；其他 Worker IPC 继续使用短超时。完成后复验普通非符号链接、0600、ZIP local header/EOCD、SHA-256 和 instance/run 绑定，每实例只保留最近三份。UI 必须先取得“包含敏感数据”的显式确认；Microdroid 不伪装支持完整 AOSP dumpstate。
- `xtask`：流程验证、质量、黑盒 smoke、证据回读、认证、打包和 PE ABI 审计，不进入运行时。

## 状态与 operation

```text
Defined/Stopped/Blocked/Failed
              │ start operation
              ▼
Preparing → StartingWorker → LaunchingGuest → NegotiatingDisplay
                                                   │
                                                   ▼
GuestBooting → AdbConnecting → Ready ⇄ Paused
      active/failed cleanup ───────→ Stopping → Stopped
host restart with live worker ─────→ Recovering → reconciled state
Stopped/Blocked/Failed ────────────→ Deleting → Deleted
```

Host 保存 desired/observed/revision；Worker 是活动运行状态的事实来源。所有改变状态的 API 创建持久化 `OperationRecordV2`，idempotency key 对同一请求返回同一 operation，对冲突请求返回稳定错误。`Ready` 只能按顺序从真实 frame 与 ADB 条件到达，不能从 Defined/Blocked 直接写入。

`Blocked` 表示供应链、认证、能力或外部运行条件不成立；`Failed` 表示输入已满足后执行失败。停止只有在 crosvm 已退出、运行 endpoint 已清理且 `cleanup_pending=false` 后才释放高风险租约。清理失败保留 PID、launch 和租约用于下一次精确协调。

## Host/Worker 所有权

Host 使用 `host.lock` 对一个 data root 实施单写者锁；锁文件从创建开始使用当前用户 ACL/权限，第二个 Host 不能替换 inode 后绕过锁。Host runtime descriptor 包含随机 loopback origin、进程启动标记、bearer 和当前 `hd-host` 可执行文件 SHA-256，使用 owner-only 原子写入；健康响应必须回显同一摘要，否则客户端拒绝该连接。

客户端同时计算自身同目录 `hd-host` 的摘要。摘要不同时，若所有实例均为非活动态，客户端通过认证 shutdown 边界回收旧 Host 及空闲 Worker，再启动并复验当前 Host；只要存在活动实例，就必须保留旧 Host/Worker/Guest，不允许以升级名义中断工作负载。UI 持续显示“停止所有实例并重新打开 HD”的升级提示，停止后下一次连接才完成切换。

每个 Worker 拥有：

- PID、进程启动标记、nonce 和当前用户隔离的 pipe/Unix socket；
- 独立 256-bit secret，Worker 对 bearer 做常量时间比较；
- 当前 crosvm handle、containment、launch plan、ADB client 和 run journal；
- frame generation、设备 endpoint 和易失句柄；
- 与 Host redb 租约绑定的精确身份。

Host/UI 退出不会因父子句柄自动杀死 Worker；crosvm 则受 Worker containment 管理，Worker 崩溃时必须终止。Host 重启读取 Worker descriptor，先验证 PID+启动标记+nonce，再认证 Ping 和协调状态；失效 descriptor 不会被当作存活实例。

每实例 `worker.lock` 由 Worker 以 OS 独占锁持有到进程退出。即使 descriptor 或 redb 中的 Worker identity 丢失，Host 也不会覆盖仍持锁的运行时，而是连接由 instance UUID 确定的 endpoint、完成 secret 认证并重建 descriptor；无法认证时保持 `Recovering` 和租约，禁止启动替换进程。

## 数据、迁移与租约

`InstanceSpecV2` 使用 `schema_version=2`、未知字段拒绝和范围校验，不允许任意 crosvm/kernel 参数逃逸。V1 文件在不跟随链接、1 MiB 限额下读取，先写逐字节相同的 owner-only backup，再把迁移后的实例和 `MigrationRecordV2` 置于同一 redb 事务；任一步失败不会覆盖源文件或留下半条数据库记录。

redb 保存实例、operation、idempotency 和 lease。一次 start 在单事务中校验所有活动实例的 CPU/内存总量，并保留 CPU、内存、Guest CID、ADB loopback port、disk overlay、GPU slot、Worker endpoint、按实例单调的 frame generation 和启用设备 endpoint；Worker 身份建立后再次事务绑定。Host 不靠进程内计数判断释放，而是回读 Worker 的停止/清理证明。

Android powerwash 是独立的可恢复存储事务，不等价于删除实例：只接受 Defined/Stopped Android，调用必须携带当前 revision、精确实例名和备份身份。Host 先终止仍驻留的 Worker 并释放 lease，再在 blocking 线程池完整计算 userdata SHA-256；原 overlay 只在同一受管数据卷内原子 rename 到 `disks/powerwash/<instance>/<backup>.img`，不会复制大型稀疏镜像。恢复前若已有新的 overlay，会先把它原子移动为新的 rollback backup，再恢复旧文件，从而允许反向恢复；产品最多保留一个可恢复备份。每次 rename 或永久 discard 前先把 `InstanceStorageTransactionV2` 写入 redb，完成后才发布新 backup 状态；Host 启动根据 overlay/source/rollback 三者组合决定安全提交或回滚，歧义组合只把该实例标记 Blocked，绝不猜测或删除文件。实例永久删除时才同时清理其受管 powerwash 目录。

Microdroid 的 `one_cpu` 按一个 vCPU 参与常规 Host CPU 预留与并行准入。显式 `match_host` 对应 AOSP AVF 的全部宿主逻辑 CPU 拓扑，必须在没有任何既有 CPU 租约时一次性租用全部逻辑 CPU；该满额租约随后自然拒绝其他 Android/Microdroid 启动，停止回收后才恢复常规准入。配置中的通用 `cpu_count=1` 只是旧格式兼容哨兵，不得用于少记 `match_host` 的实际资源需求；能力探测和 UI 必须回显独占语义。

Microdroid 上传型主 Payload 可绑定最多八个有序 extra APK。导入时 Host 在既有 ZIP32、v3/v3.1 signing-block 验证之后，以 64 KiB 解压上限有界读取 `assets/vm_config.json`，只提取 `extra_apks` 声明数量；配置保存该数量以及 Host 生成的 upload UUID 与 SHA-256，不保存或使用配置中的用户路径。数量未选满时 `microdroid.profile` 明确阻断，Worker 在创建主 idsig 和启动 VM 前重新解析签名 Payload 并以稳定 `microdroid_extra_apk_count_mismatch` 拒绝不一致；旧实例没有持久数量时也会在 Worker 边界重新解析。更换主 Payload 会清空旧集合。Worker 随后逐项复验受管文件、摘要、APK 结构和 v3/v3.1 signing block，并在 owner-only run 目录创建独立 idsig。AOSP `configPath` 原本会让 virtmgr 直接打开 APK 配置中声明的 Host 路径，桌面端口因此增加严格的 caller-opened descriptor override：数量必须与 `extra_apks` 声明精确相等，virtmgr 只 clone 已传入的描述符，仍使用签名主 APK 内的配置决定 Guest 顺序和身份。该扩展不接受任意路径字符串，也不授权 extra APEX。

有限 Microdroid Payload 的完成状态只从宿主 AOSP `vm` 启动器取证。Worker 有界读取 `microdroid.stdout.log` 和 `microdroid.stderr.log`：必须同时看到精确 `VM ended: Shutdown`、唯一且可解析的 `payload finished with exit code N`，并确认启动器进程退出 0。Guest console、virtmgr trace 和 vmclient trace 不能提供成功证据。Payload 返回 0 时，退出监控通过实例操作锁复用正式停止清理路径，依次收口录制/路线、VM、components、runtime endpoints、ADB 和瞬态文件；Worker 先原子写入绑定 instance/run 的 `Stopped / exit_code=0` result，再发布 Stopped。Host 只在无符号链接有界回读同一 result 后才把 desired 收敛为 Stopped 并释放 Host 租约；极短任务即使在首次 Ready 回读前结束也作为成功完成而非启动失败。非零返回进入稳定 `microdroid_payload_failed` 并把原始退出码写入 run result。已知 AVF death reason 在完成证据之前分类，用户主动停止和自然结束由同一操作锁串行化，因此不会形成重复清理或相互覆盖终态。

磁盘每实例独立。资源准入区分 `new_instance_storage` 与 `existing_instance_storage`：尚未分配私有存储时预留完整配置容量，已经存在 regular、非符号链接且尺寸兼容的实例存储时只要求 1 GiB 运行余量，避免每次重启重复计算已经占用的容量。`DiskProvisioner` 仍先验证源 regular file，目标已存在时验证精确大小；新建时尝试平台 block clone/reflink，无法提供该能力时执行完整文件复制并原子提交。运行配置从不持久化原生 handle。

## 能力与发布认证

运行能力指纹由平台/架构、虚拟化、按实例 CPU/内存/磁盘准入状态、工具版本、严格 frame probe 和设备 profile 共同生成；可用内存/磁盘的瞬时字节值不会进入 hash，避免 Host/Worker 采样抖动。发布身份另用稳定指纹，排除每实例资源/readiness，只绑定平台、bundle、工具、frame 和完整固定设备 profile。Guest/Host bundle 采用 Ed25519 签名、内容寻址目录、READY 标记、manifest digest 和逐文件大小/SHA-256 验证，运行时不下载或猜测文件。

启动还必须加载精确匹配的 `HostCertificationV2`。认证绑定平台、架构、Guest digest、Host digest、能力指纹、控制/frame 协议、设备 profile、签发/过期时间和八份证据 digest，并由同一信任根验证。认证不能跨 bundle、平台或能力变化重放。

正式 component 使用 `FormalComponentProbeV2`：协议版本、component id、`formal=true` 和必需 feature 必须全部匹配。仅存在一个可执行文件不代表能力可用。Worker 为每个启用的设备 component 生成 owner-only `FormalComponentLaunchV2::DeviceAdapter`，其中只授予该 component 所需的 Guest endpoint、Guest CID、VM control endpoint、独立本机 control endpoint 和每次运行随机生成的 256-bit bearer。固定 profile 为 Bluetooth、GNSS、location、UWB、NFC、sensors、MCU、network-control、audio-control 和 camera-control 分配稳定且互不复用的 virtio-console 通道；modem 使用按 Guest CID 隔离的 host-vsock 9697，network/audio/camera 的数据面仍由各正式 virtio 后端提供。component 必须以精确 launch hash、PID 和进程启动标记发布 ready marker；Worker 随后通过 `DeviceControlRequestV2` 以常量时间校验 bearer，并用 Ping 校验 instance/run/request 身份，之后定位、电池、传感器、网络、蓝牙和 NFC 动作才会路由到对应签名 component。Bluetooth Peer ID 由调用方明确指定，创建、广播和移除使用同一稳定身份。任一认证、握手、响应身份或进程存活检查失败都会阻止启动或使运行失败，不回退到 Worker 内模拟。

macOS Android 实例启用 NFC 时，Worker 必须同时创建 `nfc` Guest 串口、启动正式 Casimir component，并由运行时设备策略恢复 NFC package、启动 `nfc_hal_service` 和启用 framework NFC；不能因为 Host 平台是 macOS 而停止 HAL。实例禁用 NFC 时才移除 package 并停止 HAL。正式真实 Guest 门要求观察到非零 NCI 数据，并证明 HAL、Cuttlefish HAL 进程和 framework binder 在 Type 2、Type 4、移除动作及第二次启动后仍然存活；仅通过 component Ping 不构成 Guest NFC 可用。

macOS Android 实例启用 Bluetooth 时，Worker 必须创建 `bluetooth` Guest H4 串口、启动正式 RootCanal component，并由运行时设备策略启用 framework Bluetooth；不能再用软件 surface 或 disabled 占位 artifact 声明能力。正式真实 Guest 门要求观察到非零 H4 数据，并证明 Bluetooth HAL、framework binder 和 `ON` 状态在虚拟 GATT peer 创建、BLE 广告开关、移除动作及第二次启动后仍然存活；仅通过 component Ping 或 Host 侧动作响应不构成 Guest Bluetooth 可用。HOGP 键盘另以完整 GAP/GATT/HID、加密、bond、CCCD 与 Android input 回读证明，不复活 AOSP 已删除且不发送按键的 legacy keyboard 假能力。当前边界不声明物理 RF。

Bluetooth HCI 抓包是显式、按实例、按 run 的诊断生命周期，不是 RootCanal remote console：portable action 只携带 Host 生成的 UUID 和 1–30 秒时长，RootCanal 只记录 Guest↔Controller H4，并在当前 component 目录输出 owner-only、最大 4 MiB 的标准 btsnoop HCI UART 和 typed sidecar。Worker 必须在接受结果前复验普通非符号链接文件、UUID/文件名/时长/实际大小、标准头与上限。诊断收集器只对严格匹配 `run/<UUID>/components/rootcanal-hci-<UUID>.btsnoop`、标准头、未截断且不超过上限的文件跳过文本脱敏，避免 `from_utf8_lossy` 破坏二进制；sidecar 仍走 JSON 脱敏/规范化。诊断包的 0600、逐文件 SHA、manifest SHA 和 archive SHA 构成对外交付边界，新的 run 清除旧 capture 状态。PHY 拓扑和任意远端控制台继续阻断。

macOS Android 实例启用 UWB 时，Worker 必须创建 `uwb` Guest UCI 串口并启动正式 UWB component，不能把已打包但未连接的 adapter 降级成软件 baseline。正式真实 Guest 门要求观察到非零 UCI 数据，并证明 `android.hardware.uwb-service`、AIDL HAL、framework service、`Device state = READY` 与国家码在首次和第二次启动都成立；Host component 的 Ping 或合成 UCI contract smoke 不能替代真实 Guest HAL/framework 门。当前边界只声明确定性 FiRa v2 session 与短地址距离报告，不声明物理 RF、角度或 CCC 一致性。

macOS Android 实例启用 Modem 时，启动参数必须设置 `androidboot.modem_simulator_ports=9697`，由 Cuttlefish vendor RIL 原生创建 `AF_VSOCK` 并经 crosvm 连接正式 modem component；禁用时保持 Guest 的 no-RIL profile。正式真实 Guest 门要求唯一 Host adapter、owner-only CID socket、运行中的 vendor RIL、Radio HAL、telephony framework 与测试运营商 `00101` 在两次启动均成立。该能力只声明 SIM 身份、信号、运营商与注册查询，不声明物理运营商、通话、短信、IMS、5G 或数据附着。

Unix 受管进程停止先以精确 PID 和启动标记发送 `SIGTERM`，给正式组件五秒清理 owner-only socket 和状态文件；只有进程不响应时才强制终止。socket guard 仅删除自身绑定的 device/inode，避免路径被替换后误删。

## 本机 API 安全

HTTP server 只绑定 IPv4 loopback 随机端口。每个请求必须满足精确 Host、允许的本机 Origin（或无 Origin）和 bearer；响应禁用缓存，上传/JSON/响应均有限额，重定向和系统代理被客户端关闭。APK 上传流式写入同一个私有读写句柄，同时计算 hash；随后有界验证 ZIP32 EOCD、单磁盘、central/local header 边界及唯一 `AndroidManifest.xml`，再原子提交，过程中不按路径重开暂存文件。

Worker IPC 使用长度前缀、消息上限和 V2 协议/instance/request id 校验。设备 component IPC 使用 64 KiB 上限、超时、严格 protocol/instance/run/request id、每组件独立 256-bit bearer 和每个 component 独立 endpoint；Host 与 Worker 在转发前都执行统一的动作范围验证。Windows named pipe 拒绝远程客户端并按当前 SID 命名；Unix socket 位于用户 runtime scope 且权限为 0600。token 的 Debug 输出固定脱敏，诊断 JSON 按字段名再次脱敏，日志和 API 错误不返回 secret。

## 显示与 ADB 契约

显示链只允许平台原生外部资源：Windows Vulkan Win32 handle、Linux Vulkan dma-buf、macOS Metal IOSurface。`FrameReadyMarkerV2` 绑定 instance/run/generation、producer PID 启动标记、transport、same-adapter、memory-export、explicit-sync 和 validation-clean；producer 进程必须仍存活。`hd-frame` 对三个 buffer 的 producer/consumer 所有权和同步值做严格校验。

ADB 配置只有 Disabled 或 Loopback。Loopback port 是租约资源，最终 bridge 必须只监听 `127.0.0.1` 并把同一 Guest CID 的固定 adbd service 暴露给 `adb connect`。ADB 就绪是 `connect`、device、`sys.boot_completed=1`、`service.bootanim.exit=1`、`init.svc.bootanim=stopped`、bootanimation 进程不存在、主用户已解锁、Package Manager、SurfaceFlinger、InputManager 与 WindowManager 全部可用的连续稳定合取；随后还用镜像实际提供的 `cmd package wait-for-background-handler --timeout 5000` 有界排空 PackageManager 启动尾段后台任务。全局 `am wait-for-broadcast-idle` 不属于就绪条件，因为长期后台接收器可让它在设备已经可交互后仍不返回。退出请求只是必要条件，不能单独构成 Ready。启动设备策略和网络协调后，keep-awake 与显示配置作为幂等事务进行有界收敛：每次尝试前复验服务，瞬时撤销则重试，配置成功后再复验同一合取，避免发布启动尾段的瞬时状态。macOS 原生显示可以在 ADB 就绪前呈现启动画面，但 `adb_ready` 保持 false，ADB 动作和 APK 安装继续拒绝，直到延迟就绪任务完成最终复验后才发布可交互状态。

Unix 上“detached”只表示独立会话，不免除父进程回收责任。UI 启动 Host、Host 启动 Worker 后都由父进程内的专用 reaper 持有 `Child` 并等待退出；删除实例和 Host shutdown 必须同时满足业务进程退出与僵尸进程为零，不能依赖父进程最终退出时的被动清理。

运行历史只保留可诊断、不可再生的状态。macOS Android 启动时生成的 `initrd-android-hd.img` 是从已验签 initrd 确定性派生的单次启动输入；Microdroid extra APK 的 `microdroid-extra-0.idsig` 至 `microdroid-extra-7.idsig` 同样由受管 APK 确定性生成且只属于单次 run。Guest 停止并成功写入最终 `result.json` 后，Worker 只删除这些精确白名单文件并记录 `runtime.run.ephemeral_artifact.removed`。每次新 run 启动前的 retention 维护也对已有最终结果的历史 run 执行同一清理，使旧版本升级后遗留的可再生输入无需等到整轮历史淘汰。没有最终结果的活动 run、日志、manifest、events、实例私有磁盘和其他文件均不在清理范围；`microdroid-extra-8.idsig` 等越界名称不会被模式误删，文件类型异常或符号链接直接阻断。这样既保留失败现场，也避免 20 轮历史为每轮重复保存可再生成的启动输入。

Microdroid console input 不再使用会立即 EOF 的普通空文件。Unix Worker 在 owner-only run 目录创建并以设备/inode 身份绑定 `microdroid-console-in.fifo`，预持读写 FD 后交给 AOSP `vm --console-in`；停止、失败或 Worker 回收时只删除仍匹配原身份的 FIFO。唯一写入能力是 Full-debug 的一次性 typed nonce challenge：调用方只能提供非空 challenge UUID 和显式确认，32 字节 nonce 由 Worker 生成，固定帧不超过 160 字节，5 秒内从当前 run 的普通非符号链接 console 尾部精确验证响应。audit 只保留 nonce SHA-256，不保留原 nonce；通用 UI 不暴露入口，也没有任意 console/shell 字符串或路径参数。

当前仓库已经定义并执行这些硬门禁，并提供 Windows `hd-adb-bridge`、gfxstream external-memory producer 与 `hd-frame-producer` importer。Windows 开发机已用 Guest bundle `22281c84556c6e865e4f94498968efc2f651ef4c37bd3e871d930414609b2986` 和 Host-tools bundle `5187de8d05cf29fdbed127c72ab0a96b50fa37f800137079488237208ae1aa7e` 完成真实 Android 15 Ready、ADB 重连、四方向显示矩阵、严格零 readback/software-blit，以及 frame producer/crosvm/Worker/Host 在线退出恢复；最终 100 次生命周期与 2 小时长稳正在当前 release runtime 上重跑。Linux/macOS 原生 gate 与聚合发布认证仍不存在；没有匹配认证的制品/平台保持 Blocked，不由软件回退绕过。

## macOS Android 签名制品仓库

macOS 分发包使用可迁移的 `signed-artifact-store-v2`，不把外部 `products/android` 路径写入实例或应用配置。仓库根固定包含 `index-v2.json`、`trusted-keys-v2.json` 与 `bundles/<digest>/`；索引把 channel、数据配置、Guest/Host digest 和 Android rootfs 相对路径绑定在一起。Guest bundle 只允许 direct-linux kernel、initrd、aggregate 与 fstab，Host bundle固定 crosvm、gfxstream、ANGLE EGL/GLES 和 Vulkan loader 等运行角色。解析器先拒绝仓库根或任意父目录符号链接，再验 Ed25519 签名、逐文件摘要、fstab/data-profile 和全树精确闭包，之后才向 Worker 暴露规范化绝对路径。

development store 只能声明 `development-unencrypted`，其签名能力可以启用明确的开发数据盘绕过；release store 必须声明 `metadata-encrypted` 并通过生产信任根与 HostCertificationV2，不能继承 QA 密钥或开发绕过。UI 只在空的隔离 data root 安装包内 development QA 信任根，既有根不匹配时直接阻断。打包后的 Host 运行库搜索路径由已验签角色推导，禁止猜测源码树布局；Android 大 aggregate 通过 APFS clone/sparse tar 保持稀疏，不允许退化为稠密复制。

## 跨平台矩阵

| 边界 | Windows x64 | Linux x64 | macOS arm64 |
|---|---|---|---|
| Rust/C++ ABI | MinGW GNU only | native GNU | Apple clang/native Rust |
| 虚拟化探测 | WHPX capability | `/dev/kvm` 可读写 | `kern.hv_support` |
| 内存探测 | `GlobalMemoryStatusEx` | `sysconf` available pages | `hw.memsize` 的 75% 安全预算 |
| 本机 IPC | SID-scoped named pipe | 0600 Unix socket | 0600 Unix socket |
| 进程身份 | PID + creation FILETIME | PID + `/proc` start time | PID + `proc_bsdinfo` start time |
| containment | Job object | parent-death signal | process-control terminate-on-parent |
| 私有数据 | 当前用户+SYSTEM DACL | 0700/0600 + no-follow | 0700/0600 + no-follow |
| frame transport | Vulkan Win32 | Vulkan dma-buf | Metal IOSurface |

平台新增能力必须先形成 portable 数据/trait 和稳定失败语义，再写目标模块；不允许在 UI 中用 `cfg` 分叉状态机或把 unsupported 静默变成成功。
