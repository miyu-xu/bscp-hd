# HD 原子交付计划与完成定义

## 目标与发布原则

HD V2 以固定 Android 15 Guest、独立 Host/Worker、多实例、严格零拷贝显示、正式设备 profile 和三平台证据作为一个发布单元。仓库允许在实现和验证期间保持 `in_progress`，但不发布模拟启动、软件显示回退、未认证 bundle 或只在单平台验证的中间产品。

Windows 使用 MinGW/GNU ABI；Linux x86_64 使用 KVM；macOS arm64 使用 Hypervisor.framework。跨平台差异必须位于 `hd-platform` 或签名 Host component，不能泄漏到 `hd-core` 或 UI 业务逻辑。

## 六项 Host P0

| ID | 已落地主干 | 可执行完成证据 |
|---|---|---|
| P0-1 V2 数据与操作 | 严格 `InstanceSpecV2`、V1→V2 原子迁移/备份、redb、revision、幂等异步 operation、状态机 | all-targets 编译；migration/operation contract smoke |
| P0-2 进程与恢复 | 常驻 `hd-host`、每实例 `hd-worker`、crosvm 分层；PID+启动标记+nonce 身份；每实例 OS lock；退出监控；失败清理证明；descriptor 丢失时按认证 endpoint 恢复 | 真实 detached worker Ping/auth/Shutdown/重复 Worker 拒绝/descriptor 重建 smoke |
| P0-3 能力与供应链 | 平台/虚拟化/实例 CPU/内存/磁盘、工具 JSON probe、Ed25519 bundle、READY、逐文件 hash、精确签名认证 | capability 输出；不认证 start 必须进入 Blocked |
| P0-4 本机 API 与客户端 | loopback 随机端口、owner-only bearer descriptor、Host/Origin 校验、请求/响应限额、上传流、SSE、UI/CLI 同一客户端 | HTTP security smoke；OpenAPI V2 路由契约 |
| P0-5 事务租约 | CPU/内存总量、CID、ADB port、disk、GPU、worker、frame generation 在 redb 单事务保留/绑定/释放；frame generation 按实例单调；启动和清理失败不提前释放 | 双实例隔离与容量 smoke；lease audit；Blocked→Stop 后租约为空；重启 reconcile |
| P0-6 诊断与证据 | manifest/events/result、稳定 boundary 事件、结构化日志、诊断 tar.zst、manifest/hash/archive hash/脱敏/限额/保留 | diagnostic smoke；包自身 SHA-256 与 API 回读精确相等 |

这六项的代码和 Host contract smoke 通过，不等于 Android 发布完成。任务卷 `automation/tasks/hd-p0-v2.json` 只有在下述运行验收全部具有真实证据后才能置为 `complete`。

## 真实运行验收

### Guest 启动闭环

固定源码版本生成 Guest bundle，包含 kernel、initrd、rootfs、fstab，以及配置要求的 system/vendor 镜像。bundle 必须声明 `android-15.0.0_r14`、`hd-guest-profile-v2` 和 `hd-device-bridge-v2`，经受信密钥签名并内容寻址。

唯一 `Ready` 条件是同一次 `run_id` 同时满足：crosvm 进程存活、严格 frame generation 握手通过、ADB loopback bridge 已建立、`adb wait-for-device` 成功、`sys.boot_completed=1`、package manager 可查询。任何超时或进程退出写入稳定错误码和 `result.json`，不能推进到 `Ready`。

### 严格零拷贝显示

每个平台只接受三缓冲外部 GPU 资源和显式同步：Windows Vulkan external Win32 handle，Linux Vulkan dma-buf，macOS Metal IOSurface。producer/consumer 必须证明同一物理适配器，buffer generation 和同步值严格单调；readback、CPU copy、软件 blit、视频编码回退计数必须为零。不满足时启动被能力门阻止。

### 正式设备 profile

Host bundle 必须完整提供并探测 `hd-device-sim`、RootCanal adapter、Casimir adapter、UWB、modem、network、audio 和 camera adapter；实例开关只决定本次运行激活哪些端点，不能改变发布认证身份。每个组件返回 V2 正式 probe，Guest bundle 提供固定串口/virtio 端点契约。对外只声明 conformance 场景真实覆盖的能力；不把无 RF、secure element、IMS、carrier 或硬件认证的模拟能力写成物理设备能力。

### 三平台矩阵

| Runner | 必须通过 |
|---|---|
| Windows 11 x86_64 + WHPX + MinGW + Vulkan GPU | 冷启动、双实例、动作/APK/旋转、100 次 start/stop、zero-copy、PE audit |
| Ubuntu 24.04 x86_64 + KVM + Vulkan GPU | 同一 Guest/profile、双实例、dma-buf/显式同步、打包与长稳 |
| macOS 15 arm64 + HVF + Metal GPU | arm64 Guest、IOSurface/显式同步、双实例、打包与长稳 |

当前会话只有 Windows 开发机，且没有认证 Guest/Host bundle；因此这些 gate 状态必须保持缺失/阻塞，不能人工改写为通过。

## 自动迭代顺序

每轮固定执行：

1. 回读 `AGENTS.md`、任务卷、相关嵌套仓库状态和上轮 readback；
2. 在版本化 portable contract 中完成行为，再实现平台/运行时/UI；
3. 同步加入稳定事件、错误码、测试源码和独立黑盒场景；
4. 运行 process-check、diff check、fmt、all-targets check、Clippy、build、contract/process smoke；
5. 涉及 crosvm/gfxstream 时运行联合 MinGW 编译与 ABI 审计；
6. 在专用 runner 运行 real-guest/zero-copy/device-profile 场景；
7. 自动回读 gate log、run journal、诊断包、PE imports 和所有工作树；
8. 失败由 AI 修复并从相同任务卷重跑，直到全绿或确认是外部权限/硬件/制品阻塞。

人工只决定产品方向、Guest/组件来源、信任根、不可逆迁移和发布授权；AI 负责实现、诊断、构建、验证、回读和自动迭代。不存在需要人工手工点 UI 才能完成的发布 gate。

## 完成判定

原子交付完成必须同时满足：

- 任务卷所有 required gate 为 `pass`，没有 `missing`、`skipped` 或过期证据；
- `code_present`、`tests_authored`、`contract_smoke_verified`、`real_guest_verified` 均为 true；
- 三平台认证与当前 platform/architecture、Guest/Host digest、能力指纹精确匹配，且有效期不超过 31 天；
- 无生产模拟分支、软件显示回退、静默能力降级、孤儿 VM/Worker、租约泄漏或数据迁移损坏；
- readback 清楚保留用户既有改动，发布目录的运行时可执行文件齐全，Windows PE 不含 MSVC runtime。

任何一项不满足时，只能报告已经验证的 Host 能力和具体阻塞，不能宣称 HD V2 发布完成。
