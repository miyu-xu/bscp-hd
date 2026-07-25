# HD V2 架构与跨平台边界

## 进程与依赖

```text
hd.exe (Manager) ── launches/focuses ── hd-player.exe (one per instance)
        │                                  │ NativeDisplayTargetV2 / viewport heartbeat
        └──────────── HTTP V2 + bearer ────┘
                         │
                         ▼
     hd-host.exe ───── redb / leases / uploads / diagnostics / reconciliation
        │ authenticated WorkerProtocolV2（每实例 secret）
        ├──────── hd-worker.exe ── crosvm ── Android Guest
        ├──────── hd-worker.exe ── crosvm ── Android Guest
        └──────── ...
                         │
                         ├─ crosvm HWND becomes a child of Player's private HWND (Windows)
                         ├─ ADB readiness/actions/install/screencap
                         ├─ strict frame broker generation
                         └─ signed device components / Guest bridge
```

依赖方向固定：

- `hd-core`：V2 配置、协议、状态、制品、租约、诊断和 frame 数据；不依赖 OS、窗口或进程 API。
- `hd-platform`：数据目录安全、文件原子性、进程身份/containment、磁盘复制、资源/虚拟化探测和 `VmBackend` 等窄边界；unsafe 只在 Windows/Unix 模块。
- `hd-runtime`：Host/Worker、HTTP/IPC、redb、签名制品、租约、crosvm、ADB、journal、上传和诊断。
- `hd-frame`：三缓冲外部资源、显式同步、generation 和所有权验证；不存在像素缓冲回退接口。
- `hd-device-sim`：版本化且确定性的定位、电池、网络条件和固定传感器控制协议；蓝牙/NFC 由正式 adapter 负责。
- `hd-adb-bridge`：Windows 上的 owner-only loopback TCP 到 crosvm host-initiated guest-vsock 命名管道桥；每条 ADB 连接先通过 VM control `connect_vsock` 绑定同一 CID/guest:5555，再进行无解析字节转发。
- `hd-casimir-adapter`：把固定 AOSP Casimir NFCC/RF 引擎嵌入正式组件，双向桥接 Guest NCI 命名管道，并提供受鉴权的 Type 2 内存标签与 Type 4 NDEF/APDU 标签控制。
- `hd-rootcanal-adapter`：把固定 AOSP RootCanal `DualModeController`、LMP/LLCP Rust 引擎和 Windows BCrypt AES 嵌入正式组件，双向桥接 Guest H4 命名管道，并提供受鉴权的 BLE 广播 peer 与最小 GAP/GATT Device Name 服务。
- `hd-frame-producer`：Windows 上的严格 frame broker；按 gfxstream Vulkan 物理设备 LUID 选择同一适配器，导入三份 `VK_KHR_external_memory_win32` image memory，执行 GPU 完成 fence 到 broker release 的单调所有权协议，全程不映射像素或提供 copy/readback 回退。
- `hd-peripheral-adapters`：生成 UWB、modem、network、audio、camera 五个独立正式进程；每个进程只获授自己的 Guest control 通道和 bearer。Windows modem adapter 监听 Guest RIL 固定使用的 host-vsock 9697，经 crosvm AVF 命名管道帧桥接并提供确定性 AT baseline；UWB adapter 提供 Android 15 可解析的 FiRa v2 capability、session init/config/start/stop/deinit 状态机与短地址确定性距离报告，但不声明 RF、角度或 CCC 一致性；network/virtio-net 与 audio/virtio-snd 数据面由 crosvm 提供。
- Windows crosvm 正式运行使用 `warn` 日志级别；逐 MMIO 的 `info` 追踪会在单次启动写入数万至十万行，既不属于默认诊断证据，也会显著拖慢 Guest 启动。需要底层追踪时应在隔离的专项复现中显式提高日志级别。
- `hd-host`、`hd-worker`：独立可执行进程。
- `hd.exe`（Manager）、`hd-player.exe` 和 `hdctl`：只通过 `HostClientV2` 操作 Host，不直接持有 VM、磁盘或帧像素。

Windows 的实例画面由 crosvm `gpu_display_win` 持有原生渲染 HWND、WndProc 与输入路径，gfxstream 负责 Guest Vulkan 渲染和 color buffer，同构建启用的 `vulkan_display` 负责导入 image/semaphore 并呈现到该 HWND。crosvm 初始窗口遵守 `hidden=true`，不会先显示独立顶层窗口；Player 创建私有子 HWND，经短期、instance/generation 绑定的 DisplaySessionV2 通过 Worker 将 crosvm HWND 改为 `WS_CHILD`、按物理像素 resize，完成后才显示。Player 不把外部 Vulkan image 经 egui/wgpu/D3D12 再复制进自身窗口。Player 崩溃、心跳过期或 detach 时 Worker 隐藏并停放 crosvm 窗口，VM 与 gfxstream 保持运行，新的 Player 可重新附加。截图从 Guest ADB `screencap` 保存，不读取 Host Vulkan surface。gfxstream 在 GPU completion future 完成后，将同一 color buffer 的 Win32 external-memory handle 交给 `hd-frame-producer` 导入并等待严格 release，再完成 virtio-gpu timeline。这样显示保持同适配器、三缓冲、显式同步及零 CPU readback，软件 blit 只保留为非 Vulkan 兼容回退，不进入正常 Player 显示链路。
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

Host 使用 `host.lock` 对一个 data root 实施单写者锁；锁文件从创建开始使用当前用户 ACL/权限，第二个 Host 不能替换 inode 后绕过锁。Host runtime descriptor 包含随机 loopback origin、进程启动标记和 bearer，使用 owner-only 原子写入。

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

磁盘每实例独立。`DiskProvisioner` 先验证源 regular file，目标已存在时验证大小；新建时尝试平台 block clone/reflink，无法提供该能力时执行完整文件复制并原子提交。运行配置从不持久化原生 handle。

## 能力与发布认证

运行能力指纹由平台/架构、虚拟化、按实例 CPU/内存/磁盘准入状态、工具版本、严格 frame probe 和设备 profile 共同生成；可用内存/磁盘的瞬时字节值不会进入 hash，避免 Host/Worker 采样抖动。发布身份另用稳定指纹，排除每实例资源/readiness，只绑定平台、bundle、工具、frame 和完整固定设备 profile。Guest/Host bundle 采用 Ed25519 签名、内容寻址目录、READY 标记、manifest digest 和逐文件大小/SHA-256 验证，运行时不下载或猜测文件。

启动还必须加载精确匹配的 `HostCertificationV2`。认证绑定平台、架构、Guest digest、Host digest、能力指纹、控制/frame 协议、设备 profile、签发/过期时间和八份证据 digest，并由同一信任根验证。认证不能跨 bundle、平台或能力变化重放。

正式 component 使用 `FormalComponentProbeV2`：协议版本、component id、`formal=true` 和必需 feature 必须全部匹配。仅存在一个可执行文件不代表能力可用。Worker 为每个启用的设备 component 生成 owner-only `FormalComponentLaunchV2::DeviceAdapter`，其中只授予该 component 所需的 Guest endpoint、Guest CID、VM control endpoint、独立本机 control endpoint 和每次运行随机生成的 256-bit bearer。固定 profile 为 Bluetooth、GNSS、location、UWB、NFC、sensors、MCU、network-control、audio-control 和 camera-control 分配稳定且互不复用的 virtio-console 通道；modem 使用按 Guest CID 隔离的 host-vsock 9697，network/audio/camera 的数据面仍由各正式 virtio 后端提供。component 必须以精确 launch hash、PID 和进程启动标记发布 ready marker；Worker 随后通过 `DeviceControlRequestV2` 以常量时间校验 bearer，并用 Ping 校验 instance/run/request 身份，之后定位、电池、传感器、网络、蓝牙和 NFC 动作才会路由到对应签名 component。Bluetooth Peer ID 由调用方明确指定，创建、广播和移除使用同一稳定身份。任一认证、握手、响应身份或进程存活检查失败都会阻止启动或使运行失败，不回退到 Worker 内模拟。

## 本机 API 安全

HTTP server 只绑定 IPv4 loopback 随机端口。每个请求必须满足精确 Host、允许的本机 Origin（或无 Origin）和 bearer；响应禁用缓存，上传/JSON/响应均有限额，重定向和系统代理被客户端关闭。APK 上传流式写入同一个私有读写句柄，同时计算 hash；随后有界验证 ZIP32 EOCD、单磁盘、central/local header 边界及唯一 `AndroidManifest.xml`，再原子提交，过程中不按路径重开暂存文件。

Worker IPC 使用长度前缀、消息上限和 V2 协议/instance/request id 校验。设备 component IPC 使用 64 KiB 上限、超时、严格 protocol/instance/run/request id、每组件独立 256-bit bearer 和每个 component 独立 endpoint；Host 与 Worker 在转发前都执行统一的动作范围验证。Windows named pipe 拒绝远程客户端并按当前 SID 命名；Unix socket 位于用户 runtime scope 且权限为 0600。token 的 Debug 输出固定脱敏，诊断 JSON 按字段名再次脱敏，日志和 API 错误不返回 secret。

## 显示与 ADB 契约

显示链只允许平台原生外部资源：Windows Vulkan Win32 handle、Linux Vulkan dma-buf、macOS Metal IOSurface。`FrameReadyMarkerV2` 绑定 instance/run/generation、producer PID 启动标记、transport、same-adapter、memory-export、explicit-sync 和 validation-clean；producer 进程必须仍存活。`hd-frame` 对三个 buffer 的 producer/consumer 所有权和同步值做严格校验。

ADB 配置只有 Disabled 或 Loopback。Loopback port 是租约资源，最终 bridge 必须只监听 `127.0.0.1` 并把同一 Guest CID 的固定 adbd service 暴露给 `adb connect`。Worker 只有在 `connect`、device、boot-completed 和 package-manager probe 全部成功后保存 client 并进入 Ready；常规动作和 APK 安装在其他状态一律拒绝。

当前仓库已经定义并执行这些硬门禁，并提供 Windows `hd-adb-bridge`、gfxstream external-memory producer 与 `hd-frame-producer` importer。Windows 开发机已用 Guest bundle `22281c84556c6e865e4f94498968efc2f651ef4c37bd3e871d930414609b2986` 和 Host-tools bundle `5187de8d05cf29fdbed127c72ab0a96b50fa37f800137079488237208ae1aa7e` 完成真实 Android 15 Ready、ADB 重连、四方向显示矩阵、严格零 readback/software-blit，以及 frame producer/crosvm/Worker/Host 在线退出恢复；最终 100 次生命周期与 2 小时长稳正在当前 release runtime 上重跑。Linux/macOS 原生 gate 与聚合发布认证仍不存在；没有匹配认证的制品/平台保持 Blocked，不由软件回退绕过。

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
