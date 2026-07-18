# 架构与跨平台边界

## 分层

```text
hd-ui / hdctl
      │ ControlProtocolV1
      ▼
hd-runtime Supervisor ── RunJournal / Artifact validation / ADB
      │ traits + versioned launch contracts
      ▼
hd-platform ───────────── CrosvmBackend
      │                         │
      │ native display lease    │ process/control socket
      ▼                         ▼
winit-owned host window      crosvm ── gpu_display ── gfxstream
```

依赖只向下：

- `hd-core`：纯数据契约、验证和状态机，不依赖窗口、进程或操作系统 API。
- `hd-platform`：显示、进程、磁盘、guest port 与 VM backend traits；Win32 实现在独立模块。
- `hd-runtime`：多实例监督器、构件验证、私有磁盘、ADB、IPC、遥测与运行证据。
- `hd-ui`：eframe/winit/egui/wgpu 界面，不直接启动 crosvm 或操作磁盘。
- `hdctl`：同一 IPC 协议的无界面客户端。
- `xtask`：质量、烟测、PE ABI 审计和打包，不进入产品运行时。

## 实例状态机

```text
Defined/Stopped/Failed/Blocked
            │ start
            ▼
        Preparing → Launching → DisplayAttached → GuestBooting
                                                   │
                                                   ▼
                                            AdbConnecting → Ready
            active states ───────────────→ Stopping → Stopped
            startup error ───────────────→ Failed or Blocked
```

`Blocked` 表示缺少外部输入或环境条件，例如 guest 构件不存在；`Failed` 表示已有输入下的执行失败。状态包含单调 revision 和原因。真实启动目前最多推进至 `GuestBooting`，直到 M3 的 ADB/boot readiness probe 完成。

## 配置与运行数据

`InstanceConfigV1` 是可持久化配置，使用 `schema_version`、未知字段拒绝和范围验证。原生窗口句柄、进程句柄、pipe handle、临时端口租约等只存在于 `InstanceRecord`/`PlatformDisplayLease`，不得序列化。

每次 start 生成新 `run_id`：

- `manifest.json`：配置快照、完整命令/环境、显示 lease 的非原生契约、构件 SHA-256 和工具链指纹；
- `events.jsonl`：带时间、序号、类别、名称、状态和字段的低频事件；
- `result.json`：起止时间、最终状态、退出码和原因；
- stdout/stderr：crosvm 原始输出。

构件验证失败也必须生成 manifest/events/result。日志严禁逐帧写入，避免 I/O 改变渲染行为。

## 多实例所有权

一个 `Supervisor` 持有多个 `InstanceRecord`。每个活动实例独立拥有：

- UUID、状态 revision 和运行 journal；
- crosvm child process；
- UI 容器窗口 lease；
- GPU telemetry endpoint/task；
- 私有可写 rootfs 副本；
- ADB serial；手工指定的宿主端口在配置写入时做跨实例唯一性校验，自动端口租约与 guest bridge 在 M3 完成。

Supervisor 启动时对 data root 的 `supervisor.lock` 获取跨进程独占锁并写入 PID，防止两个 UI 同时修改同一实例配置或磁盘。

停止会终止 child、取消 telemetry task、清除易失句柄并写终态。实例切换只移动/缩放容器窗口，不把 HWND 写入配置。删除配置前必须是终态且无 child。

## Windows 显示链

1. eframe/winit 创建 HD 顶层窗口，wgpu 固定 DX12。
2. `NativeDisplayEmbedder` 创建 `STATIC` 子 HWND 并持有其生命周期。
3. `CrosvmBackend` 在 `--gpu` 中传递十进制 `parent-window-handle`。
4. crosvm 参数把句柄视为 launch-only；Windows broker 交给 `WindowProcedureThreadBuilder`。
5. gpu_display 使用 `WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS` 创建 gfxstream 窗口。
6. 显示替换通过版本化 crosvm control command，在同一 scanout id 下释放旧 surface 并装入新参数。

HD 容器 HWND 必须比 crosvm 子窗口活得更久。活动实例不能替换 display lease；先停止实例再释放容器。

## VSync 与 FPS

`HD_VSYNC` 是每个 crosvm 进程的启动环境：

- on：仅接受 Vulkan FIFO；
- off：IMMEDIATE → MAILBOX → FIFO，落到 FIFO 时记录一次告警。

因此 VSync 是启动参数，运行中变更返回需重启。gfxstream 只在 `vkQueuePresentKHR == VK_SUCCESS` 后计数，每约一秒发送 `GpuStatsEventV1`。Supervisor 验证协议版本，且只在实例启用“显示宿主 FPS”时保存值。

## 跨平台适配表

| 能力 | 抽象 | Windows | Linux/macOS 当前状态 |
|---|---|---|---|
| 数据目录 | `DataPaths` | LocalAppData | 平台 local data dir |
| 显示嵌入 | `DisplayEmbedder` | 子 HWND | passthrough 契约；原生接管待 M4 |
| 进程 | `ProcessSupervisor` | Tokio child | 同一实现 |
| 私有磁盘 | `DiskProvisioner` | block clone/reflink，失败全拷贝 | 同一策略，需平台回读 |
| VM | `VmBackend` | crosvm/WHpx | trait 已固定，后端验收待 M4 |
| 本机控制 | IPC adapter | SID-scoped named pipe | Unix domain socket |
| ADB bridge | `GuestPortBridge` | 契约已留出 | M3/M4 实现 |

平台新增能力必须先扩展 trait/版本化协议，再在目标模块实现；禁止在 UI 中散布 `cfg(windows)` 业务逻辑。
