# HD 运行与故障处理

## 启动与连接

发布目录中的五个 HD exe 必须保持同目录。启动 `hd.exe` 或任意不带 `--no-start-host` 的 `hdctl` 命令时，客户端先验证现有 `host-runtime-v2.json` 和进程身份；没有有效 Host 时拉起同目录 `hd-host.exe`。Host 在实例 start 时拉起同目录 `hd-worker.exe`。

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

`--data-root <PATH>` 选择隔离数据根；`--no-start-host` 要求只连接而不拉起 Host。UI 退出只断开客户端，Host 继续运行。没有活动实例时 `hdctl shutdown` 可退出 Host；存在活动实例时它会拒绝，必须显式使用 `hdctl shutdown --stop-all`。

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

Host exit monitor 会协调 Worker；Worker exit monitor 会写 run result。回读 instance 的 `worker` identity、Worker descriptor、`child_pid`、`cleanup_pending` 和 lease audit。只有精确 PID+启动标记匹配时才允许终止，防止 PID 重用误杀。

若 Worker descriptor 丢失但 `worker.lock` 仍被持有，Host 会通过确定性 endpoint 和实例 secret 认证 Worker 并重建 descriptor；不要手工删除 lock 或租约。endpoint 无法认证时实例保持 `Recovering`，先保留现场并生成诊断。

### Windows ABI/缺 DLL

目标必须为 `x86_64-pc-windows-gnu`，五个 HD exe、crosvm 和依赖 DLL 必须来自同一 MinGW 发布链。运行 `build.bat` 或根 `build_all.bat`；不要用 MSVC DLL 补齐。保留 `objdump -p` PE audit 输出。

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
