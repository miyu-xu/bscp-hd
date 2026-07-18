# 运行与故障处理

## 启动方式

从统一发布目录启动 `bin\hd.exe`。开发树可运行 GNU release 产物。UI 默认启用 Mock 启动；真实启动前先在实例设置中填写构件路径并运行“诊断”。

`hdctl` 连接正在运行的 `hd.exe`：

```powershell
hdctl ping
hdctl list
hdctl show <UUID>
hdctl start <UUID> --mock
hdctl action <UUID> home
hdctl action <UUID> recent
hdctl action <UUID> back
hdctl install <UUID> app.apk
hdctl diagnose <UUID>
hdctl stop <UUID>
```

UI 进程拥有 supervisor；关闭 UI 会停止所有实例并请求 control server 退出。data root 有跨进程独占锁，第二个使用同一 `HD_DATA_DIR` 的 HD UI 会拒绝启动；不要删除正在使用的 `supervisor.lock`。

删除已停止实例会删除该实例的配置和私有磁盘，这是不可恢复操作；历史 `runs/<UUID>` 证据会保留，便于审计和故障回读。

## Mock 验收

1. 新建两个实例并设置不同名称、CPU、内存和分辨率。
2. 分别 Mock 启动；状态应到 Ready。
3. 切换实例、Home/Recent/Back/Rotate，并用一个任意 regular file 验证 mock APK 路径。
4. 停止两个实例。
5. 在 data root 的 `runs` 下确认每次启动都有 manifest/events/result。

自动等价流程：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- smoke
```

## 真实启动前置条件

实例至少需要非空：

- kernel；
- initrd；
- writable base rootfs；
- android fstab；
- 可选 system/vendor image；
- 可选每个命名构件的 SHA-256。

HD 会为每个实例创建私有 rootfs，优先 block clone/reflink，失败时完整复制。务必为 data root 留出一份完整镜像的空间。HD 不下载、解压或构建这些文件。

当前 M3 未完成时，真实 VM 即使成功 spawn 也只显示 `GuestBooting`；ADB bridge/readiness 未建立前 Home/APK 等会明确返回 ADB unavailable。

## 常见故障

### 状态为 Blocked

运行 `hdctl diagnose <UUID>`，检查构件路径、文件大小、checksum、crosvm 与 ADB。查看最近 run 的 `events.jsonl` 和 `result.json`。修正设置后可以直接再次 start；也可 stop 把状态归一为 Stopped。

### VSync 修改提示需重启

这是预期行为。VSync 通过每个 crosvm 进程的 `HD_VSYNC` 在 swapchain 创建时选择 present mode。停止实例、保存设置并重新启动。分辨率/DPI/刷新率/方向可走运行时显示事务。

### FPS 不显示

确认实例设置中“宿主 FPS”已启用，且真实 gfxstream 已产生成功 Vulkan present。Mock 不生成虚假 FPS。检查 crosvm 环境中的 `HD_GPU_STATS_PIPE`/`HD_GPU_STATS_INSTANCE` 和应用日志中的 telemetry warning。

### 窗口空白或脱离主界面

确认使用包含 HD crosvm 改动的二进制，`--gpu` 中存在 `parent-window-handle`，且 gfxstream backend DLL 与 crosvm 架构一致。检查 crosvm stderr。不要复用上一次进程的 HWND；停止实例后由 UI 重新创建 lease。

### ADB unavailable

阶段 0 下属于已知阻塞。M3 完成后检查 ADB enabled、端口租约、guest bridge、`adb connect`、guest adbd 和 readiness 事件。若配置禁用 ADB，控制动作不会自动绕过该设置。

当前若关闭“自动端口”，必须填写宿主端口，而且同一 data root 下各实例不可重复；保存时返回 `adb_port_conflict` 表示该端口已被其他实例占用。“自动端口”仅保留配置语义，实际租约和 serial 建立仍属于 M3，不会提前宣称 ADB 可用。

### Windows 构建链接异常

确认：

```powershell
rustup target list --installed
C:\workspace\mingw64\bin\gcc.exe --version
C:\workspace\mingw64\bin\objdump.exe --version
```

目标必须包含 `x86_64-pc-windows-gnu`。不要用 MSVC 产物补齐缺失 DLL；重新运行 `build.bat` 或根 `build_all.bat`，并保留 PE audit 输出。

## 证据打包

报告单次失败时提供对应 run 目录及当天 `logs/hd.jsonl.*`，先检查其中是否含敏感本地路径。至少保留：instance UUID、run UUID、manifest、events、result、crosvm stdout/stderr、HD/crosvm/gfxstream commit 和 MinGW/Rust 版本。不要只截取 UI 状态文字。
