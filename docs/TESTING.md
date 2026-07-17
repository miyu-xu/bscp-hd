# HD 质量、证据与回读

## 执行约束

仓库明确要求不运行 unittest。因此禁止 `cargo test`、`cargo nextest`、C++ gtest/ctest 和 Python unittest。测试源码仍随功能维护，并由 `cargo check --workspace --all-targets` 和 Clippy 编译；可执行行为由独立黑盒 smoke、进程场景、集成构建和专用 real-guest runner 验证。只有人工明确修改仓库约束后才能启用测试 harness。

## Host 质量门

Windows：

```powershell
cd hd
cargo run --target x86_64-pc-windows-gnu -p xtask -- quality
```

`xtask quality` 固定执行：

1. `process-check`：校验 AGENTS、V2 schema、样例、所有任务卷和脚本中的 unittest 禁令；
2. 工作区与暂存区 `git diff --check`；
3. `cargo fmt --all -- --check`；
4. MinGW `cargo check --workspace --all-targets`；
5. MinGW `cargo clippy --workspace --all-targets -- -D warnings`；
6. MinGW `cargo build --workspace --bins`；
7. 独立 `xtask smoke`。

`xtask smoke` 当前以临时私有 data root 执行真实文件、数据库、HTTP 和进程边界，覆盖：

- V1 文件迁移到 V2、原始 backup 和 redb 结果；
- 单 data root 的 Host 独占锁；
- Host runtime descriptor、健康身份和客户端连接；
- 实例创建、列表、持久化与幂等 start operation；
- 缺失认证时 start 必须失败并进入 `Blocked`，不能产生 Ready；
- 无 bearer、恶意 Origin 和错误 Host 的 HTTP 拒绝；
- 两实例 CPU/内存/CID/ADB/GPU/磁盘/Worker/frame generation 租约隔离、generation 单调、audit 及完全释放；
- Blocked 实例 stop、租约完全释放和实例删除；
- APK 流式上传、ZIP/APK 结构校验与诊断 archive/hash 精确回读；
- Host shutdown 后 descriptor 清理；
- 真正 detached `hd-worker` 进程、PID/启动标记/nonce、实例 OS lock、重复 Worker 拒绝、正确 secret Ping、错误 secret 拒绝、descriptor 删除后认证重建、认证 Shutdown、进程退出和 descriptor 清理。

这个 smoke 被命名为 contract/process smoke，只证明 Host 控制面和硬阻断正确，不生成 Android 运行成功证据。

## 构建与平台门

Windows release：

```bat
build.bat
```

它构建整个 workspace，并用 MinGW `objdump -p` 审计所有 exe；任何 `VCRUNTIME`、`MSVCP`、`CONCRT` 或 `MFC` import 都阻断。根 `build_all.bat` 还编译 bscp/crosvm/可选 gfxstream，并验证发布目录包含五个 HD 运行时 exe。

portable Rust 编译：

```bash
cargo check --workspace --all-targets --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --target aarch64-apple-darwin
```

交叉编译失败若来自缺少目标 linker/SDK，只能记录为 runner/toolchain 阻塞；不能把 Windows 编译结果填写为 Linux/macOS pass。最终平台门必须在原生 runner 执行 check、Clippy、build、package 和运行场景。

## 自动证据闭环

推荐入口：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- ai-cycle `
  --task automation/tasks/hd-p0-v2.json `
  --output out/ai/hd-p0-v2
```

无论门禁成功或失败，输出均保留：

- `logs/<gate>.log`：完整命令输出；
- `hd-gates.json`：版本化 gate report；
- `readback.json`：机器事实底稿；
- `readback.md`：人类可读摘要；
- 每个相关仓库的 HEAD/dirty 状态。

涉及 crosvm/gfxstream/根发布链时：

```powershell
.\scripts\integration-quality.ps1 `
  -Task automation/tasks/workspace-integration.json `
  -Output out/ai/integration
```

脚本只编译 C++/Rust 测试目标，不运行测试 harness。`-RunRootBuild` 明确授权后才执行完整根构建；已有脏文件不会被自动清理、reset 或覆盖。

## 真实 Android 与零拷贝矩阵

每个平台专用 runner 必须自动执行：

1. 从空 data root 加载固定签名 Guest/Host bundle并回读所有 capability；
2. 冷启动到唯一 Ready 条件，保存 manifest/events/result 和诊断包；
3. 两实例并行，验证 CID/ADB port/disk/GPU/frame/设备 endpoint 隔离；
4. Home、Recent、Back、Power、Volume 的 Guest UI/系统状态回读；
5. APK 安装后用 package manager 回读精确包路径；
6. 四种方向、分辨率、DPI、30/60/90/120 Hz 与 Android rotation 一致，失败事务回滚；
7. VSync on/off、同适配器外部纹理、显式同步、三缓冲 generation，CPU readback/software blit/编码计数均为零；
8. 定位、电池、网络、五类传感器、RootCanal、Casimir 及启用 adapter 的 profile conformance；
9. crosvm/Worker/Host 异常退出、ADB 超时、坏签名/hash、磁盘不足、端口占用和 frame producer 退出的故障注入；
10. 100 次启动停止、2 小时长稳、资源/句柄/任务/日志上限。

平台 gate 生成原始证据文件，`xtask certify` 只在八类聚合证据名称精确匹配时签发最多 31 天的认证：`hd_quality`、`host_worker_smoke`、`http_security_smoke`、`lease_recovery_smoke`、`diagnostic_smoke`、`real_guest`、`zero_copy`、`device_profile_conformance`。认证不执行测试，也不能替代证据；它只把已经通过的证据 digest 绑定到当前 bundle/能力。

## 回读与判定

每轮必须回读：

- 每个命令退出码和 warning；
- required gate 是否全部存在且为 pass；
- `readback.json` 与任务卷 evidence state；
- 所有相关嵌套仓库 `git status --short` 和 diff check；
- run `manifest.json` 的 bundle/launch/toolchain，`events.jsonl` 的序号/边界/错误，`result.json` 的终态；
- Worker/Host descriptor 与进程身份、redb lease 和 cleanup_pending；
- 诊断 manifest/hash/脱敏结果；
- Windows PE imports；
- real-guest、zero-copy、device profile 的原始 runner 证据及签名有效期。

证据状态严格分层：代码存在、测试源码已编译、contract smoke 通过、真实 Guest 通过。前三项不能推出第四项；UI 按钮、协议类型、Blocked 行为或人工截图也不能推出真实功能通过。
