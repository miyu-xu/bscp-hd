# HD 运行与故障处理

## 启动与连接

发布目录中的十五个 HD exe 必须保持同目录。`hd.exe` 是多实例 Manager，双击实例或使用“打开 Player”会启动 `hd-player.exe --instance-id <UUID>`；重复打开会激活已有 Player。二者与不带 `--no-start-host` 的 `hdctl` 都会先验证现有 `host-runtime-v2.json` 和进程身份；没有有效 Host 时拉起同目录 `hd-host.exe`。Host 在实例 start 时拉起同目录 `hd-worker.exe`。签名 Host bundle 必须登记 ADB、Casimir、RootCanal、frame producer 以及 UWB/modem/network/audio/camera 的对应正式角色。

```powershell
hdctl health
hdctl capabilities
hdctl list
hdctl create --name Android-1
hdctl show <UUID>
hdctl capabilities <UUID>
hdctl start <UUID>
hdctl action <UUID> key home
hdctl action <UUID> key recent
hdctl action <UUID> key back
hdctl action <UUID> rotate landscape
hdctl install <UUID> app.apk
hdctl diagnostics --instance-id <UUID>
hdctl stop <UUID>
```

`hd.exe --data-root <PATH>`、`hd-player.exe --instance-id <UUID> --data-root <PATH>` 与 `hdctl --data-root <PATH>` 选择同一个隔离数据根；不传时都使用 `%LOCALAPPDATA%\bscp\hd`。打开 Player 会自动提交实例启动；关闭 Player 会先释放显示会话，再对该实例执行正常关机，实例停止后窗口才退出。需要无界面后台实例时使用 `hdctl` 启动，不通过关闭 Player 隐藏。`--no-start-host` 要求 CLI 只连接而不拉起 Host。没有活动实例时 `hdctl shutdown` 可退出 Host；存在活动实例时它会拒绝，必须显式使用 `hdctl shutdown --stop-all`。

## 创建可启动实例

默认实例没有 artifact selection，start 必然进入 `Blocked`。可启动 spec 必须通过 `hdctl update` 或 UI 设置精确选择：

- artifact store root；
- 64 位小写十六进制 Guest bundle digest；
- 64 位小写十六进制 Host bundle digest；
- CPU、内存、显示、ADB、boot 和 device profile。

store 布局：

```text
<store>/bundles/<digest>/manifest-v2.json
<store>/bundles/<digest>/READY-v2.json
<store>/bundles/<digest>/<manifest files>
```

不要手工编写 manifest/READY。取得固定构建产物和授权 Ed25519 密钥后，用 `xtask publish-bundle` 从明确的 `role=relative/path` 清单生成 bundle；命令先在同一 store 的 staging 目录完成逐文件 hash、签名、trust 验证和完整 resolver 复验，全部通过后才按 digest 原子发布：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- publish-bundle `
  --kind host-tools --input-root C:\artifacts\host --store-root C:\hd-store `
  --platform windows --architecture x86_64 `
  --source-manifest-digest <64_HEX> --signer-key-id release-2026 `
  --signing-key C:\keys\release.key --trust-store C:\hd-data\trusted-keys-v2.json `
  --capability hd-host-tools-v2 `
  --file crosvm=bin\crosvm.exe --executable-role crosvm
```

Guest 使用 `--kind guest --platform android`，并逐项登记 `kernel`、`initrd`、`rootfs`、`android_fstab` 及可选 `system_image`/`vendor_image` role。Host bundle 必须登记运行时要求的全部正式工具角色；缺一项时 resolver 会拒绝启动。

### 导入 Android x86_64 Cuttlefish 发布包

Google 标准 Cuttlefish image ZIP 不能直接作为 HD Guest bundle。Android 15 路径使用
同一 build 的 target-files ZIP 提供严格匹配的 vendor fstab。导入器不联网、不替换
输出、不读取密钥，也不执行签名、认证或 Guest 启动：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- import-cuttlefish `
  --image-zip D:\artifacts\aosp_cf_x86_64_phone-img-<BUILD>.zip `
  --target-files-zip D:\artifacts\aosp_cf_x86_64_phone-target_files-<BUILD>.zip `
  --sensor-injector D:\artifacts\guest\hd-sensor-injector `
  --output D:\artifacts\hd-staging\<BUILD>
```

对已经人工确认接受的 latest 构建，可提供官方 OTA metadata 文本；导入器会校验
build/SDK/x86_64/userdebug，并从 `vendor_boot.img` 的 legacy-LZ4 ramdisk 提取 fstab：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- import-cuttlefish `
  --image-zip D:\artifacts\aosp_cf_x86_64_only_phone-img-<BUILD>.zip `
  --ota-metadata D:\artifacts\aosp_cf_x86_64_only_phone-ota_metadata-<BUILD>.txt `
  --output D:\artifacts\hd-staging\<BUILD>
```

当前已验证的输入组合为 Android 15/SDK 35 和 Android 17/SDK 37 的 x86_64
userdebug Cuttlefish。
导入器拒绝 ZIP 路径穿越、symlink、重复关键镜像、缺失分区、错误 boot header、
错误 sensor-injector ABI 和已有输出目录。Android sparse `super`/`userdata` 会在
组盘前流式展开；发布包缺少 `metadata.img` 时会生成 64 MiB 可格式化空分区；输出
GPT 的 disk/partition GUID 由所有输入 digest 确定生成。

输出：

```text
<output>/guest/kernel
<output>/guest/initrd_android.img
<output>/guest/aggregate_android.img
<output>/guest/android_fstab.dt
<output>/guest/hd-sensor-injector       # 提供 --sensor-injector 时生成
<output>/import-manifest-v1.json
<output>/publish-request-v1.json
<output>/import-events.jsonl
```

`publish-request-v1.json` 只是后续发布参数底稿。输出明确保持
`production_ready=false`；只有转换后的 Guest 通过 real-guest、zero-copy 和
device-profile gate 后，人工才可授权其声明对应 Android capability、
`hd-guest-profile-v2`、`hd-device-bridge-v2` 并调用 `publish-bundle`。新 bundle
digest 还需要重新签发匹配的 Host certification。未提供 sensor-injector 的输出会带
`sensor_injector_missing` blocker。Android 17 输出会记录
`cuttlefish-android17-split-sensors-v1`（20 个 virtio-console，hvc18 控制、hvc19
数据）并带 `hd_runtime_android17_split_sensors_profile_not_integrated` blocker；在正式
runtime 和 Host bundle 接入该 profile 前禁止发布。

Android 17 build `15885347` 的 Windows/WHPX 手工 smoke 结果如下：导入后的 GPT、
dynamic partitions、AVB/dm-verity、EROFS、metadata/userdata 格式化和 SELinux 均可工作；
补齐该版本的 multi-install vendor APEX selectors 后，`apexd` 成功扫描并激活 87 个
APEX，随后 adbd、zygote、音频、网络和多个 HAL 可以启动。因此发布 image ZIP 的
磁盘内容不需要重打包为另一套 Android 分区格式，但仍不能直接声明为 HD Ready。
当前 HD 正式 gfxstream guest-ANGLE/ranchu 路径在 SurfaceFlinger 初始化 virtio-gpu
context 时仍会出现 `DRM_IOCTL_VIRTGPU_GET_CAPS`/`DRM_IOCTL_VIRTGPU_CONTEXT_INIT` 错误和
WHPX soft-lockup。Windows 手工 smoke 已改用官方 Cuttlefish `guest_swiftshader` 合约：
crosvm `backend=2d`，Guest 使用 ANGLE/Pastel、minigbm 和 ranchu composer。为此 Windows
crosvm 允许显式 `Mode2D`，非 gfxstream 构建也补齐了显示窗口代码的 `anyhow::Context`
导入。build `15885347` 在该路径上已实测 `sys.boot_completed=1`、
`init.svc.bootanim=stopped`，SurfaceFlinger 识别 `CrosvmDisplay`，PackageManager 可查询
151 个包。转换后的分区无需重排，导入器生成的 Android 17 initrd 也固定采用这组
Guest bootconfig。

这条路径使用 Guest 软件 Vulkan，只证明发布镜像可转换并完整启动；它不满足 HD 正式
硬件渲染/严格零拷贝契约，也没有把 Android 17 split-sensors profile 接入正式 resolver，
因此不能据此移除 `hd_runtime_android17_split_sensors_profile_not_integrated` blocker 或
发布认证能力。

同一次诊断确认 `androidboot.openthread_node_id=1` 可消除 Thread HAL 的 `node_id=0`
退出；lights 还要求按实例 Guest CID 动态注入 `androidboot.vsock_lights_cid` 和
`androidboot.vsock_lights_port`，不能固化在可多实例复用的 initrd 中。对 build
`15885347` 对齐 `android17-release` 源码和官方 `launcher.log` 后，确认其布局不是旧版
`/dev/hvc13` 单通道，而是 `/dev/hvc18` sensors control 与 `/dev/hvc19` sensors data 两个
双向通道。Windows 20-console smoke 已完成两组命名管道连接、Sensors HAL 协议握手并注册
`android.hardware.sensors.ISensors/default`；完整启动后 SensorService 识别 3 个 Goldfish
硬件传感器并持续收到事件。ADB 已确认 Android 17/SDK 37、build `15885347`，
`sys.boot_completed=1`。此前的 rutabaga `-22`/SurfaceFlinger binder 阻塞已由 2D
Guest SwiftShader 路径关闭；剩余工作是正式 HD profile/resolver、动态 lights 参数、
sensor-injector、zero-copy 与 certification gate。

`cvd-host_package.tar.gz` 是 Linux host tools，不是 OTA metadata，也不是 HD Guest
bundle；Windows HD 不能直接执行其中的 ELF `launch_cvd`/`crosvm`。它可用于核对同一
build 的官方组装参数。在 Linux 主机上直接运行官方包仍要求 KVM 等受支持的 VM
manager；WSL 环境没有 `/dev/kvm` 时无法运行 Guest，但同 build 的 host package 可用于
生成并核对 assembly/launcher 配置。

不需要真实镜像即可执行独立结构 smoke：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- import-cuttlefish --self-check
```

data root 还必须有 `trusted-keys-v2.json`，包含受信 Ed25519 public key。Host 会验证签名、manifest/READY、逐文件大小/hash、平台/架构/capability、正式 component JSON probe，以及当前 bundle/能力对应的未过期签名认证。运行时不联网下载或补齐缺失内容。

在 start 前运行：

```powershell
hdctl capabilities <UUID>
hdctl diagnostics --instance-id <UUID>
```

只有 `certified=true` 且所有 required probe 和启用设备均 available 才具备启动资格。

## 生命周期语义

- `start`、`stop`、`restart`、`pause`、`resume`、`delete`、`display`、`install` 和 diagnostics operation 都持久化，可用 `hdctl operations` / `hdctl operation <OP_UUID> --wait` 回读。
- 重复的内部 idempotency key 不会创建第二个操作；CLI 默认等待，`--no-wait` 返回 operation 后立即退出。
- Graceful stop 先发送 Guest power button 并等待配置时限，再精确终止 crosvm；`--force` 直接终止。
- `Stopped` 意味着 child 不存在、endpoint 已清理、`cleanup_pending=false`。若清理失败则保持 Failed 和租约，不能手工删 lock/pipe 冒充修复。
- `restart_policy=on_failure` 会处理两类在线故障：组件/crosvm 失败时复用已清理的 Worker 启动新 run；精确 Worker identity 死亡时释放旧租约并创建替代 Worker。自动恢复最多连续尝试 3 次，同一失败 revision 只排队一次；达到 Ready 或用户 stop 后计数清零。Blocked 不自动重试。
- Host 异常退出不终止 detached Worker/VM。新 Host 必须通过 descriptor/endpoint/secret 和 PID+启动标记认证后重连同一 run；不能把 Host 重启误实现为 Guest 重启。
- 删除仅允许非活动实例；它删除实例配置、私有磁盘、Worker 数据和该实例全部 run history，不可恢复。先生成诊断并另行归档所需证据。

## 常见阻塞

### `release.certification` 或 `artifact.*` Blocked

读取 `hdctl capabilities <UUID>` 中每个 probe 的 detail/properties。常见原因是 trust store 缺失、bundle digest/签名/READY/hash 不匹配、Host bundle 平台不符、正式 component probe 失败、认证与能力指纹不匹配或过期。不要复制另一平台认证或修改 JSON；重新生成对应 runner 证据并由授权密钥签发。

### `host.resources` Blocked

probe 同时列出 logical/requested CPU、total/available/requested memory、Host reserve、memory source 和可用磁盘。start 还会在同一 redb 事务中汇总全部活动实例的 CPU/内存租约，避免并发准入越限。降低实例规格或停止其他实例以释放租约；不能绕过 reserve。macOS 使用总内存 75% 的确定性安全预算。

### `display.zero_copy` Blocked

Host bundle 的 `frame-producer --probe-v2 --json` 没有证明当前平台要求的 external memory、explicit sync、same adapter 和 validation clean。HD 没有软件 blit/readback 回退。更新驱动/选择同一 GPU/换用已认证 Host bundle，然后重新产生证据。

### start 到 `NegotiatingDisplay` 后失败

检查该 run 的 `frame-ready-v2.json`、crosvm stderr 和 events。marker 必须绑定相同 instance/run/generation/transport，producer PID+启动标记仍存活，并证明严格零拷贝。超时、旧 generation 或 producer 退出都会失败并触发精确清理。

### `AdbConnecting` 失败

确认租约 port 未被其他进程占用、ADB 只连接 `127.0.0.1:<port>`、Guest adbd service/bridge 与同一 CID 对应。查看 `adb connect`、wait-for-device、boot property 和 package-manager probe 错误。HD 不尝试公网地址或系统代理。

### Home/APK/设备动作被拒绝

动作要求状态为 Ready。APK 还要求上传 hash 与 Worker 重新计算一致，并在安装后从 package manager 回读包路径。蓝牙/NFC 要求对应正式 adapter 控制面；不存在时返回稳定 device adapter 错误，不会本地伪造成功。

### Host 无法启动或第二个 Host 被拒绝

同一 data root 只能有一个 Host。先用 `hdctl --no-start-host health` 检查；不要删除存活 Host 的 `host.lock`。若 descriptor 存在但进程身份无效，客户端会拒绝并拉起新 Host，Host 自身在持有锁后原子更新 descriptor。

### Worker/VM 异常退出

Worker exit monitor 会写 run result 并清理受管进程树；Host 的周期刷新会消费 Failed 或丢失的精确 Worker identity，并按上述有界策略恢复。回读 instance 的 `worker` identity、Worker descriptor、`child_pid`、`cleanup_pending` 和 lease audit。只有精确 PID+启动标记匹配时才允许终止，防止 PID 重用误杀。

Windows 可重复故障注入入口：

```powershell
.\scripts\windows-fault-injection.ps1 -InstanceId <UUID> -Output out\windows-fault-injection
```

runner 依次终止 frame producer、crosvm、Worker 和 Host，验证新 run/frame generation、替代 Worker、Host 同 run 重连、严格 Ready/零拷贝以及最终 Stopped 清理。开发迭代默认继承 `HD_DEV_FAST_ARTIFACTS=1`，证据明确标记未重复执行 capability/bundle 校验。

若 Worker descriptor 丢失但 `worker.lock` 仍被持有，Host 会通过确定性 endpoint 和实例 secret 认证 Worker 并重建 descriptor；不要手工删除 lock 或租约。endpoint 无法认证时实例保持 `Recovering`，先保留现场并生成诊断。

### Windows ABI/缺 DLL

目标必须为 `x86_64-pc-windows-gnu`，十五个 HD exe、crosvm 和依赖 DLL 必须来自同一 MinGW 发布链。运行 `build.bat` 或根 `build_all.bat`；不要用 MSVC DLL 补齐。保留 `objdump -p` PE audit 输出。

## 日志与诊断

运行数据：

```text
runs/<instance>/<run>/manifest.json
runs/<instance>/<run>/events.jsonl[.1...]
runs/<instance>/<run>/result.json
runs/<instance>/<run>/crosvm.stdout.log
runs/<instance>/<run>/crosvm.stderr.log
logs/host-v2.jsonl.*
logs/ui-v2.jsonl.*
logs/worker-<instance>-v2.jsonl.*
logs/lease-audit-v2.jsonl
```

生成诊断：

```powershell
hdctl diagnostics --instance-id <UUID>
hdctl diagnostics --instance-id <UUID> --include-guest-logs
```

Guest logs 只在 Ready 且 ADB 可用时采集。诊断包为受限大小的 `.tar.zst`，包含 manifest 和逐文件 SHA-256；API 另返回最终 archive SHA-256，并对 secret/token/authorization 等字段做大小写不敏感脱敏。报告问题时提供诊断包、archive hash、instance/run/operation UUID、HD/crosvm/gfxstream commit、Rust/MinGW 版本和对应 gate report，不只提供 UI 截图。
