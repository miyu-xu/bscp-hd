# HD Android Desktop

HD 是 `bscp` 内的独立 Rust 子仓库，用桌面窗口管理一个或多个 Android/cuttlefish 风格虚拟机实例。Windows 是首个可运行目标，Windows 构建统一使用 `x86_64-pc-windows-gnu` 与项目现有 MinGW-w64；平台协议、状态机、存储、进程与显示嵌入边界均保留 Linux/macOS 实现入口。

## 当前基线

阶段 0 已完成可编译、可烟测的产品骨架：

- `eframe + winit + egui + wgpu` 桌面 UI；Windows 强制使用 wgpu DX12 后端。
- 单一 `hd.exe` 监督器管理多实例；`hdctl.exe` 通过本机 IPC 使用同一版本化协议。
- 实例创建、持久化、启动、停止、删除、诊断与 mock 生命周期。
- 主页、最近任务、返回、电源、音量、旋转、APK 安装入口。
- CPU、内存、分辨率、DPI、刷新率、方向、VSync、宿主 FPS、ADB 与 kernel 参数设置。
- Windows 子 HWND 嵌入：HD 创建并拥有容器窗口，crosvm/gpu_display 创建其子窗口；原生句柄不写入配置。
- crosvm 同 scanout 的显示替换命令，以及 gfxstream Vulkan present mode 选择和成功 present FPS 遥测。
- 每次启动的构件指纹、启动计划、状态事件、crosvm stdout/stderr 和终态结果记录。
- 可重复 MinGW 构建、Clippy、测试目标编译、非 unittest 烟测、PE 运行库审计和打包入口。

真实 Android 启动仍是显式阻塞项。HD 不下载或构建 guest 构件；缺少 kernel/initrd/rootfs/fstab 时进入 `Blocked`，不会伪造 `Ready`。ADB guest bridge 与真实启动就绪探针需在目标 cuttlefish 构件契约确定后完成。详见 [开发计划](docs/PLAN.md) 和 [架构](docs/ARCHITECTURE.md)。

## 快速开始

Windows 本地构建：

```bat
cd hd
build.bat
```

项目统一构建：

```bat
build_all.bat
```

质量门禁（遵守仓库约束，不运行 unittest）：

```powershell
cd hd
.\scripts\quality.ps1 -WindowsGnu
```

仅运行独立流程烟测：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- smoke
```

启动 `hd.exe` 后默认勾选 Mock 启动，可在没有 Android 构件时验收多实例、设置、动作和日志流程。另一个终端可使用：

```powershell
hdctl list
hdctl start <INSTANCE_UUID> --mock
hdctl action <INSTANCE_UUID> home
hdctl stop <INSTANCE_UUID>
```

## 数据与日志

默认数据根目录：

- Windows：`%LOCALAPPDATA%\bscp\hd`
- Linux/macOS：平台本地 data directory 下的 `bscp/hd`
- 覆盖：设置环境变量 `HD_DATA_DIR`

目录契约：

```text
instances/<instance-id>/instance.json
disks/<instance-id>.img
runs/<instance-id>/<run-id>/manifest.json
runs/<instance-id>/<run-id>/events.jsonl
runs/<instance-id>/<run-id>/result.json
runs/<instance-id>/<run-id>/crosvm.stdout.log
runs/<instance-id>/<run-id>/crosvm.stderr.log
logs/hd.jsonl.<date>
```

日志不按帧写入。gfxstream 只按约一秒聚合一次成功 present，并通过版本化事件上报 FPS。

## 约束摘要

- Windows 交付物只能使用 GNU/MinGW ABI，不提供 MSVC 回退，也不允许混合链接。
- VSync 在 gfxstream swapchain 创建时生效，运行中修改会返回“需重启”，不会假装热更新成功。
- 分辨率、DPI、刷新率和方向使用 crosvm 显示替换事务；Android 方向设置失败时回滚宿主显示。
- 各实例使用独立私有磁盘；优先 block clone/reflink，不支持时完整复制。
- IPC 仅面向本机；Windows pipe 名按当前 SID 隔离并拒绝远程客户端。
- HD 验证外部 guest 构件及可选 SHA-256，不负责下载或构建它们。

## 文档索引

- [计划与完成定义](docs/PLAN.md)
- [架构与跨平台边界](docs/ARCHITECTURE.md)
- [开发和构建](docs/DEVELOPMENT.md)
- [测试、质量与回读](docs/TESTING.md)
- [完全 AI 开发流程](docs/AI_WORKFLOW.md)
- [AI 自动迭代资产](automation/README.md)
- [运行与故障处理](docs/RUNBOOK.md)
