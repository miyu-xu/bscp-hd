# HD Android Desktop

HD 是 `bscp` 内的独立 Rust 子仓库，为固定 Android 15/cuttlefish 风格 Guest 提供跨平台桌面管理、多实例和自动化控制。Windows 全链只使用 `x86_64-pc-windows-gnu` 与 MinGW-w64；Linux x86_64、macOS arm64 通过同一数据契约和窄平台适配层实现。

## 当前实现

六项 Host P0 已进入 V2 主干：

- V2 配置、V1 原子迁移、严格校验、redb 持久化和异步幂等 operation；
- 独立常驻 `hd-host`、每实例独立 `hd-worker`、crosvm 子进程和进程身份回读；
- 虚拟化、CPU/内存/磁盘、签名 Guest/Host bundle、正式组件和严格零拷贝能力探测；
- 仅 loopback 的 bearer HTTP V2 API、Host/Origin/大小边界、UI 与 `hdctl` 客户端；
- CPU/内存总量、CID、ADB 端口、磁盘、GPU、worker 与 frame generation 的事务租约和重启协调；
- 带 manifest、SHA-256、脱敏、限额与保留策略的一键诊断包，以及运行 journal。

桌面入口包含实例创建/保存/启动/暂停/恢复/停止/重启/删除，Home、最近任务、返回、电源、音量、旋转和 APK 安装，以及 CPU、内存、分辨率、DPI、刷新率、方向、VSync、宿主 FPS、ADB、启动参数、制品和设备 profile 设置。设备控制协议包含定位、电池、网络条件、传感器、RootCanal 蓝牙 peer 和 Casimir NFC tag。

生产路径没有模拟启动或软件显示回退。启动只有在以下条件同时成立时才允许继续：签名且内容寻址的 Guest/Host bundle、正式组件探测、宿主能力、严格同 GPU 外部纹理与显式同步探测，以及与当前平台/架构/bundle/能力指纹精确匹配的签名发布认证。缺少任何一项都会稳定进入 `Blocked`，不会生成虚假 `Ready`。

## 交付状态

当前工作机已验证 Host V2 契约、HTTP 安全、持久化迁移、租约清理、诊断打包、真实 `hd-worker` 进程分离，并已用 `D:\hd-v2-artifact-store` 中 Guest bundle `22281c84556c6e865e4f94498968efc2f651ef4c37bd3e871d930414609b2986` 与 Windows Host-tools bundle `5187de8d05cf29fdbed127c72ab0a96b50fa37f800137079488237208ae1aa7e` 启动 `android-15.0.0_r14` 到严格 Ready。Windows 四方向/720p–1080p/240–480 DPI/30–120 Hz、零 readback/software-blit、30 项设备/APK/双实例、frame producer/crosvm/Worker/Host 退出恢复、最终版本 100/100 生命周期和真实 120 分钟（237 个采样）长稳均已有通过证据；最终 MinGW 质量门、17 个发布 EXE 的 PE audit、8 门签名 Windows certification 及 `dev_fast=false` 的认证启动也已通过。Windows 规划功能已经收口；原子跨平台发布仍未验收，因为 Linux、macOS 原生平台 gate 与三平台聚合 certification 尚未生成，Windows 单平台完成不能替代这些最终证据。

## 快速开始

Windows MinGW release 构建与 PE ABI 审计：

```bat
cd hd
build.bat
```

项目统一构建会发布十五个 HD Windows 运行时：多实例 Manager（`hd.exe`）、每实例 Player（`hd-player.exe`）、CLI/Host/Worker、设备模拟器、ADB bridge、RootCanal/Casimir、frame producer，以及 UWB、modem、network、audio、camera 五个正式 peripheral adapter。

```bat
build_all.bat
```

不运行 unittest 的完整 Host 质量门：

```powershell
cd hd
cargo run --target x86_64-pc-windows-gnu -p xtask -- quality
```

独立黑盒契约/进程 smoke：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- smoke
```

固定 Guest/Host 产物可通过 `xtask publish-bundle` 做逐文件 hash、Ed25519 签名、READY 生成、生产 resolver 自验证和内容寻址原子发布；具体参数见 [运行手册](docs/RUNBOOK.md)。

Android x86_64 Cuttlefish 的官方 image ZIP 可通过 `xtask import-cuttlefish` 转换为
未签名 HD Guest staging。Android 15 使用匹配的 target-files ZIP；已明确接受的
latest 构建也可使用 OTA metadata，并从 vendor_boot 提取 fstab。导入不会自动声明
正式 Guest capability；用法和验证边界见 [运行手册](docs/RUNBOOK.md)。

启动 UI 或 CLI。二者都会连接现有 Host，或在不存在时拉起同目录的 `hd-host`：

```powershell
hd.exe --data-root D:\hd-v2-data
hd-player.exe --instance-id <INSTANCE_UUID> --data-root D:\hd-v2-data
hdctl --data-root D:\hd-v2-data health
hdctl --data-root D:\hd-v2-data create --name Android-1
hdctl --data-root D:\hd-v2-data list
hdctl capabilities <INSTANCE_UUID>
hdctl start <INSTANCE_UUID>
hdctl action <INSTANCE_UUID> key home
hdctl stop <INSTANCE_UUID>
```

未配置认证 bundle 时，`start` 返回能力阻塞是正确且可诊断的行为。

## 数据与安全边界

默认数据根目录为 Windows `%LOCALAPPDATA%\bscp\hd`，Linux/macOS 的平台 local-data 目录下 `bscp/hd`；可用 `HD_DATA_DIR` 覆盖。私有目录/密钥在创建时即使用当前用户权限，敏感读取拒绝符号链接或 reparse point，持久化替换使用原子写入。

```text
host-v2.redb
host.lock
host-runtime-v2.json
instances/<instance-id>/
workers/<instance-id>/worker-v2.json
workers/<instance-id>/worker.key
workers/<instance-id>/worker.lock
disks/<instance-id>.img
runs/<instance-id>/<run-id>/manifest.json
runs/<instance-id>/<run-id>/events.jsonl
runs/<instance-id>/<run-id>/result.json
uploads/<upload-id>.apk
diagnostics/*.tar.zst
certifications/*.json
logs/*-v2.jsonl.*
```

Host 数据根由 `host.lock` 单写者保护；UI 退出不会停止 Host 或活动 Worker。`hdctl shutdown` 只退出 Host，`hdctl shutdown --stop-all` 才先停止实例。删除实例会删除该实例配置、磁盘、Worker 元数据和运行目录，是不可恢复操作。

## 关键约束

- Windows 禁止 MSVC fallback 或混合 ABI；发布目录必须通过 PE import audit。
- 运行时不下载 Guest、工具或设备组件；只接受受信 Ed25519 签名、READY 标记和逐文件 SHA-256 验证通过的 bundle。
- 官方 Cuttlefish image ZIP 与 Linux `cvd-host_package.tar.gz` 都不是可直接启动的 HD bundle；必须先离线导入，并为对应 Android 版本通过 real-guest、设备拓扑和认证门禁。
- VSync 是启动参数；不能真实热应用的变更返回需重启。分辨率、DPI、刷新率和方向走 crosvm 显示事务，Guest 方向失败时回滚。
- 严格零拷贝不支持时阻止启动；没有 readback、软件 blit、视频编码或像素缓冲回退 API。
- 所有常规 Guest 动作只在唯一 `Ready` 条件成立后执行；APK 会先流式上传、校验 SHA-256 和有界 ZIP32/唯一 manifest 结构，再由 ADB 安装并回读包路径。
- 仓库约束为不运行 unittest；测试目标仍必须编译，可执行验证使用独立 smoke、集成构建和专用 real-guest runner。

## 文档

- [原子交付计划与完成定义](docs/PLAN.md)
- [V2 架构与跨平台边界](docs/ARCHITECTURE.md)
- [开发与 MinGW 构建](docs/DEVELOPMENT.md)
- [质量、证据与回读](docs/TESTING.md)
- [完全 AI 开发闭环](docs/AI_WORKFLOW.md)
- [运行与故障处理](docs/RUNBOOK.md)
- [机器可读自动化资产](automation/README.md)
