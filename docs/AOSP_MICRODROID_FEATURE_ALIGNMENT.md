# HD 与 AOSP Microdroid 功能对齐

## 基线与范围

本矩阵以 `packages/modules/Virtualization` 的 `android-platform-15.0.0_r14` 为语义基线；本地标签解析到 `ab4bb5ac23f4aab5892ff5e550185cea1bb43891`。主要参考：

- [Android 15 Virtualization 模块](https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/tags/android-platform-15.0.0_r14/)
- [Android 15 `vm` 命令实现](https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/tags/android-platform-15.0.0_r14/android/vm/src/run.rs)
- [Android Virtualization Framework 文档](https://source.android.com/docs/core/virtualization)

HD 的 Microdroid 产品目标是 macOS Apple Silicon 与 Windows x86_64 上的按实例、无图形工作负载，不伪装成 Android 设备模拟器。macOS 使用 AVF/HVF 的 `vm -> virtmgr -> crosvm` 链路；Windows 使用已移植的 x86_64 `vm -> virtmgr -> crosvm` 链路和 `vsoc_x86_64` 运行闭包。新建入口必须由 Host 平台能力驱动，不能再通过 WebView user-agent 把已验证的 Windows 后端隐藏；其他平台仍由 UI 与 Host 双重拒绝。

状态定义：

- `verified-macos`：已由安装包中的真实 Microdroid Guest 回读；
- `verified-windows-dev`：已由 Windows GNU 开发候选和真实 x86_64 Microdroid Guest 回读，但尚未升级为签名安装包发布证据；
- `implemented`：配置、运行时和 UI 已实现，但仍缺该项独立真实 Guest 证据；
- `partial`：只覆盖 AOSP 能力的安全子集；
- `blocked`：缺少可发布实现、制品或安全证据；
- `non-goal`：发布产品明确不暴露的开发接口。

## 功能矩阵

| AOSP Microdroid 能力 | HD 当前状态 | 证据/缺口 | 优先级 |
|---|---|---|---|
| `run-microdroid` / EmptyPayload | verified-macos / verified-windows-dev | macOS 安装包真实 Guest 到 `Ready`；Windows GNU 的可重复 `windows-microdroid-real-guest` 门使用 `vsoc_x86_64` 连续三轮真实 Guest 在 1.857–2.125 秒到 `Ready`，每轮分别回读 Payload 验签、Guest boot completed、Host Payload Ready、唯一 run、有效 CID 与完整 `Host → Worker → vm → virtmgr → crosvm` 进程链；force stop、delete、Host shutdown 后残留为 0。UI 明确显示系统内置 EmptyPayload，不再误标为上传 APK 的 `assets/vm_config.json`。原始三轮证据为 `out/ai/windows-microdroid-ui-capability-20260806-v4-c3/result.json`；修复上传型配置解析后又由 `out/ai/windows-microdroid-empty-regression-20260806-final/result.json` 完成回归 | P0 |
| `run-app` / Payload APK | verified-macos / verified-windows-dev | Mac 已证明上传 APK、SHA-256 复验、`assets/vm_config.json` 校验、idsig 创建和真实 Guest `Ready`。Windows 公共 payload-config 源码现与 Mac 对齐，接受 AOSP 标准 `microdroid_launcher`、`executable`、缺省 task type 和省略空 `apexes`；Windows GNU 使用 x86_64 本地库和 v3 签名的 Payload 连续三轮执行 `run-app`，Host 上传 SHA 与源文件一致，manifest 只引用受管 upload、`payload.idsig` 和标准 config path。最新 Full-debug 回归还在同一持久实例的三轮中逐次回读回环 ADB `device`、SDK 35、Guest power down、Stopped 与零残留，Ready 为 4.870–5.077 秒、ADB Ready 为 3.944–3.989 秒；证据为 `out/ai/windows-microdroid-uploaded-full-debug-20260806-r15/result.json` | P0 |
| 每实例 Payload、调试级别和存储 | verified-macos | Android 与 Microdroid 类型不可变；多实例运行目录、Payload、日志、Guest CID、ADB 租约和 Worker 均已证明隔离 | P0 |
| `--debug full/none` | verified-macos / verified-windows-dev | `Full` 与 `None` 均进入版本化配置和真实启动参数；build133 的 Mac 独立 `None` Guest 以 `--debug none` 在 1.228 秒到 `Ready`。Windows GNU runner 现对调试级别参数化并逐对断言 manifest；`windows-microdroid-none-20260809-r5/result.json` 的真实 x86_64 Guest 在 3.563 秒到 Ready，ADB serial 为 null、`adb_ready=false`、console/Guest log 均为 0，并由 force stop、delete、`shutdown --stop-all` 无泄漏回收 | P0 |
| 回环 ADB 与正常关机 | verified-macos / verified-windows-dev | v2 运行闭包携带原始 `com.android.adbd` APEX；`Full` 下 Empty 与上传 Payload 均通过实例独立回环端口提供 ADB，Payload Ready 不等待后台探测。Darwin 接受连接继承 `O_NONBLOCK` 导致仅握手后停滞的问题已通过显式恢复阻塞数据流修复。build142 从最终归档独立解包后，单实例 Empty/Uploaded 和并发两实例均在 0–1 秒内 `adb_ready=true`，包内 ADB shell 回读 SDK 35；五个 Full-debug 停止场景都按 AOSP `adb root` 合约恢复 privileged adbd 后发出固定关机请求，Guest 记录 `reboot: Power down`、状态收敛为 Stopped，forced termination 为 0。Windows 使用同一 Full-debug 语义：桌面 virtmgr 从受信 product 的 `/system/apex` 与 `/system_ext/apex` 构造工厂 APEX 列表并挂载 `com.android.adbd`，同一命名管道句柄以 overlapped 全双工读写承载 vsock↔TCP 桥，`adb root` 后关闭旧 TCP transport 以确定触发重连。`out/ai/windows-microdroid-empty-20260806-r14/result.json` 使用同一持久实例连续三次冷启动，三轮均回读 `device`、SDK 35、Guest `reboot: Power down`、Stopped 和残留进程 0；Ready 为 4.818–5.133 秒，ADB Ready 为 3.925–4.141 秒。Android 仍保持其独立的非提权 Cuttlefish shell 关机路径；`None` 不发布端口或 adbd policy | P0 |
| console 与 Guest log | verified-macos | 每次 run 使用 owner-only 受管文件；工作负载页和诊断只读取当前实例/run。build142 的五份 Guest power-down console、Full/None 日志和 run 绑定均已由最终归档门禁保留 | P1 |
| `console-in` | implemented | AOSP `vm` 的独立 console input FD 现绑定到每 run 的 owner-only FIFO，Worker 预持读写端避免启动即 EOF。产品协议只允许 Full-debug、显式确认的一次性 `HD_CONSOLE_CHALLENGE_V1`：Host 生成 32 字节随机 nonce，固定请求 126 字节、最长等待 5 秒，只接受同一 challenge/nonce 的精确 `HD_CONSOLE_RESPONSE_V1`，不接收调用方文本或 shell；Android、None-debug、nil id、第二次发送和未确认请求均拒绝，通用 Web UI 继续隐藏。Mac 合成受信消费者黑盒已证明 0600、响应验证、超时、替换攻击拒绝、audit 不留原 nonce 和 FIFO 清理；证据为 `microdroid-console-challenge-macos-v150.json`。最终包 runner `macos-microdroid-console-challenge-smoke.sh` 已通过语法、必需断言和进程作用域静态审查；仍需专用受信 Payload 在真实 Guest 消费并回应，停止后复验 FD/进程，因此不能升级为 `verified-macos` | P1 |
| 持久加密存储 | verified-macos / verified-windows-dev | `--storage` 映射 AVF `encryptedStorageImage`，Guest 通过 dm-crypt 以读写方式挂载。Mac 已证明重启复用且另一实例获得不同 ext4 UUID 和密文摘要。Windows `windows-microdroid-full-storage-20260809-r2/result.json` 连续三轮证明 64 MiB Host 文件路径/创建身份不变、首轮且仅首轮格式化、三轮 ext4 UUID 均为 `ad445d28-f0bf-4a75-8d6f-fcf6c7c3d9dc`，每轮正常关机后记录密文 SHA-256；10–4096 MiB、普通非符号链接和禁止原地缩放已约束 | P0 |
| 内存配置 | verified-macos / verified-windows-dev | 每实例 `--mem`；Mac 最终包启动和 100 次生命周期门已覆盖。Windows runner 对 `memory_mib` 参数化，并在上述三轮真实 Guest manifest 中逐轮精确回读 `--mem 512` | P0 |
| 单 vCPU | verified-macos / verified-windows-dev | 默认 `--cpu-topology one_cpu`，允许在 Host 资源准入范围内并行运行实例；Windows 上述三轮真实 Guest逐轮精确回读该参数并保持完整 VMM 进程链 | P0 |
| `match_host` CPU 拓扑 | implemented | 每实例可显式选择 AOSP `--cpu-topology match_host`；Host 按全部逻辑 CPU 建立独占租约，只允许在 CPU 租约为空时启动，并由同一满额租约阻止其他 Android/Microdroid 并行启动。资源能力回读明确显示逻辑 CPU 数、`exclusive_cpu_lease=true`，UI 标为独占且变更要求重启；仍需 HVF 可用后补齐真实 Guest 拓扑回读、one_cpu/match_host 基准、热/功耗和停止后租约释放证据 | P2 |
| 多实例并行 | verified-macos | Empty 与 Uploaded Payload 并行 Ready；停止其中一个不影响另一个，端口/CID/日志/运行目录均唯一 | P0 |
| 有限 Payload 自然完成与退出码 | verified-windows-dev / implemented-awaiting-real-guest-macos | 对齐 AOSP `vm run`：只有宿主 `vm` 的 stderr 精确报告一次 `payload finished with exit code N`、stdout 精确包含 `VM ended: Shutdown`，且 `vm` 进程自身退出码为 0 时才接受自然完成。Windows 的真实 x86_64 v3 Payload 分别返回 0 和 17，均未发布 Ready 就在约 4.1 秒自然结束；0 收敛为 `Stopped / exit_code=0`，17 收敛为 `Failed / microdroid_payload_failed / exit_code=17`，两者 VM 子进程和最终作用域残留均为 0。过程中修复了两个产品竞态：进程退出后对继承 stdout/stderr 的后代给予最多 2 秒有界落盘窗口；启动操作仍持有实例锁时直接复用 locked stop cleanup，避免重新加锁。严格证据与失败优先级未放宽，正常 Ready/ADB 路径由 `windows-microdroid-empty-20260806-r16-regression` 回归通过。Windows 材料、成功与非零证据分别为 `windows-microdroid-finite-payload-materials-20260806-r1`、`windows-microdroid-finite-exit0-real-guest-20260806-r3`、`windows-microdroid-finite-exit17-real-guest-20260806-r1`。arm64 的 exit 0/17 ELF、仅 v3 签名 APK、zipalign、config、证书摘要和零私钥残留已在 Mac 独立复核，证据为 `macos-microdroid-finite-payload-materials-20260806-r1`；`macos-microdroid-finite-payload-smoke.sh` 已补齐最终包双实例门禁、严格日志/manifest/result 校验和 data-root 作用域清理，仍需在不影响受保护实例的可用 HVF 槽位执行真 Guest | P0 |
| 停止、删除与进程清理 | verified-macos / verified-windows-dev | Mac 100 次 Ready→stop→delete 平均 1 秒、最大 2 秒，FD/线程无增长。Windows Full-debug 三轮均由 Guest `reboot: Power down` 正常关机，None-debug 明确强制停止；两门 delete 与 Host shutdown 后按隔离 data root 过滤的相关进程均为 0 | P0 |
| Guest/Worker/Host 故障恢复 | verified-macos | 三类故障注入均通过，Guest 与 Worker 使用新 run 身份恢复，Host 故障时保留活跃 Worker/run | P0 |
| Payload 签名/idsig death reason | verified-macos | 保持 ZIP 与 v3 signing-block 结构、仅破坏 v3 signed-data digest 的真实制品通过 Host 有界预检后，由 Guest 返回 `MicrodroidPayloadVerificationFailed`；实例与 run 均保持 `Blocked / microdroid_payload_verification_failed` 和可操作修复提示 | P1 |
| Payload 身份变化 death reason | verified-macos / verified-windows-dev | 原始有效 v3 Payload 在同一实例先到 Ready；语义等价但独立重新签名的另一份有效 v3 Payload 通过 Host 预检后，由 Guest 返回 `MicrodroidPayloadHasChanged`，实例与 run 均保持 `Blocked / microdroid_payload_changed`。Windows 桌面工厂 CAPEX 解压路径是每次启动的临时目录；Payload 元数据现固定使用原始 product APEX/CAPEX 时间戳，并按模块名和预安装路径确定性排序，避免临时文件创建时间或目录枚举顺序伪造身份变化。错误分类先识别 `Payload/APEXes have changed` 的具体原因，再处理外层通用 verification 文本，不再误报为 APK 签名/idsig 错误；同一实例三轮通过证据为 `out/ai/windows-microdroid-empty-20260806-r14/result.json` | P1 |
| Guest 运行时 death reason | verified-macos | 故障门杀死真实 crosvm VMM，AVF 返回 `Killed`，原 run 收敛为 `failed / microdroid_runtime_failed`；同一 Worker 新 run、Worker 替换和 Host 重连均恢复 | P1 |
| 无效 Payload 配置 death reason | verified-macos | 与 AOSP `MicrodroidTests.bootFailsWhenConfigIsInvalid` 相同的 no-task 配置被放入结构有效、独立 v3 签名的 APK；Host 预检接受后 Guest 返回 `MicrodroidInvalidPayloadConfig`，实例与 run 均保持 `Blocked / microdroid_invalid_payload_config` | P1 |
| Guest 连接 VirtualizationService 失败 | verified-macos | 隔离 virtmgr 为本次 CID 创建的 owner-only Binder-RPC socket 在 Guest 启动前被精确移除；`microdroid_manager` 真实连接失败并返回 `MicrodroidFailedToConnectToVirtualizationService`，实例与 run 均保持 `Blocked / microdroid_service_connection_failed`。已建立连接后的 `VirtualizationServiceDied` 是运行时基础设施故障，不能误报为启动连接失败 | P1 |
| pVM / `--protected` | blocked | 当前只发布 non-protected VM。macOS 端口帮助信息不能证明硬件保护、持久 KeyMint、远程证明或与 Android pVM 等价，因此 UI 不暴露该选项 | P0 |
| 硬件 KeyMint、远程证明与生产密钥 | blocked | 开发签名和 development-unencrypted 证据不能升级为发布认证；需要稳定硬件身份、生产 Ed25519 根、撤销和证明链 | P0 |
| hugepages、boost-uclamp | non-goal-macos | 当前 macOS crosvm 虽解析 `--hugepages`，Darwin `use_hugepages()` 明确为空操作；发布 crosvm 也不提供 Linux/Android 专用的 `--boost-uclamp`。HD 不暴露无效性能开关，也不以参数字符串宣称支持；未来 Linux 产品另行按真实 THP/uclamp 计量 | — |
| extra APK/idsig | verified-windows-dev / implemented-awaiting-real-guest-macos | 上传型主 Payload 可按 `assets/vm_config.json` 声明顺序绑定最多 8 个额外 APK；Host 导入以 64 KiB 解压上限读取声明数量并跟随实例保存，UI 显示声明/已选数量，未选满由能力探测阻断，Worker 在创建 idsig 和 VM 前重新解析并返回稳定 `microdroid_extra_apk_count_mismatch`。每项使用独立受管 upload UUID/SHA-256，启动前复验 APK 结构、v3/v3.1 signing block 与摘要，并生成 owner-only idsig。桌面 `vm`/virtmgr 扩展要求 override 数量与 config 声明精确相等，只传递已打开的文件描述符，不开放配置内任意 Host 路径；更换主 Payload 自动清空旧集合。Windows GNU 的 stored/deflated inspection 黑盒及损坏/超限/空路径负向已通过；Windows/macOS runtime-storage 黑盒已证明完成 run 清理 0–7 idsig、保护活动 run 且不误删越界名称。Windows x86_64 真 Guest又完成两次独立 run：两个受管 extra APK 的 descriptor/idsig 数量、SDK 35、root 后 Guest marker 顺序、sysfs `extra-apk-0/1` mapper、跨重启 upload/SHA 稳定、Guest power down、Stopped、finished-run idsig 清理和零进程残留全部通过，证据为 `windows-microdroid-extra-apk-real-guest-20260806-r6/result.json`。材料生成器现在区分模板布局：native-first 且 Stored 的 x86_64 模板只原位替换 config；原生库被压缩或不是首项的 arm64 模板则重建为 native-first、Stored、4 KiB 页对齐后再签名。Mac 已从移动后的 `/Users/developer/Workspace/products/microdroid` 原始 EmptyPayload/VmLauncher 模板重建 `macos-microdroid-extra-materials-20260806-r2`，三份 APK 均通过 v3、zipalign、SHA、config 顺序和零私钥复核，主 Payload 原生库数据偏移为 16384。`macos-microdroid-extra-apk-smoke.sh` 真实 Guest 仍等待安全 HVF 槽位，因此 Mac 状态不提前升级 | P2 |
| extra APEX / `prefer_staged` | blocked-safe | 当前签名运行时闭包只允许认证 APEX 集合。Payload 预检会有界解析 AOSP `apexes` 与 `prefer_staged` 字段，非空 APEX 请求和 Host-staged 解析均在上传及 Worker 启动复验时明确拒绝，不能受宿主环境变量影响或复用 extra APK 开关绕过。未来支持仍需要版本化 APEX 选择、兼容性矩阵、逐项摘要、回滚与 Guest mount 证据 | P2 |
| GDB、earlycon 和任意调试端口 | non-goal | 发布 UI 不开放任意 Host 调试端口；需要时只能进入隔离的开发构建和显式诊断流程 | — |
| 图形、旋转、FPS、设备模拟 | non-goal | Microdroid 是无图形工作负载；这些设置和 Player 工具仅属于 Android 实例 | — |
| suspend/resume、snapshot/restore | blocked | 当前明确拒绝 pause/resume；持久快照还需内存、磁盘、Payload 身份、加密存储和 AVF 状态的一致性协议 | P2 |

## 当前发布门禁

1. macOS Apple Silicon 与 Windows x86_64 必须由同一个 Host 能力字段显示 Microdroid 新建入口并通过同一个 Host 创建门；其他平台保持 UI 隐藏和 Host 拒绝。Windows 能力不得再由 `navigator.userAgent` 推断。
2. 在现有跨重启与跨实例密文隔离门禁上，继续补充由专用 Payload 写入内容后的回读和删除后不可恢复证据。
3. 安装分发门必须同时证明 `debug=full` 下 Empty/Uploaded 的延迟 ADB Ready 与真实 shell 数据面，以及 `debug=none` 下 serial null、`adb_ready=false`、无 adbd policy；不得用 `adb get-state` 的宿主缓存代替 Guest shell 回读。
4. 在 pVM 硬件保护、KeyMint 和证明链完成前，禁止新增“受保护 VM”开关或发布声明。
5. `console-in` 的 Host FIFO、固定帧、一次性随机 nonce、显式确认、5 秒超时和无通用 UI 契约已经实现；只有专用受信 Payload 在真实 Guest 回应同一 nonce、停止后无 FD/进程/FIFO 泄漏时才能升级为 `verified-macos`。不允许用合成消费者、launch manifest、FIFO 存在或 crosvm 参数字符串替代 Guest 数据面证据，也不开放任意 shell UI。
6. Windows 与 macOS 的 Microdroid 测试继续使用独立 data root；不得影响正在运行的 Android Player、gfxstream 零拷贝路径或受保护的历史实例。
7. `macos-microdroid-death-reason-smoke.sh`、`macos-microdroid-payload-changed-smoke.sh`、`macos-microdroid-invalid-config-smoke.sh`、`macos-microdroid-service-connection-smoke.sh` 与 `macos-microdroid-fault-injection-smoke.sh` 必须对最终安装包分别保留真实 Guest 的签名拒绝、实例 Payload 身份绑定、no-task 配置拒绝、启动 RPC 连接失败与 VMM 运行时死亡证据；每项都必须验证 typed instance/run 结果、owner-only 证据和隔离进程清理。
8. `match_host` 候选必须在空 CPU 租约下回读 Guest 可见 CPU 数等于 Host 逻辑 CPU 数，并证明存在任一 Android/Microdroid CPU 租约时启动被拒绝、运行期间其他实例被拒绝、停止后满额租约释放；同时保留 `one_cpu` 对照的启动时间、Payload 运行时间、Host CPU、温度与功耗证据，不能只检查启动参数。
9. extra APK 候选必须使用主 Payload 配置声明的精确顺序，证明数量少/多、重复 upload、摘要变化、无 v3/v3.1 签名和 config 身份变化均被拒绝；成功路径必须在 Guest 回读每个 `/mnt/extra-apk/<index>` 的预期内容与 fs-verity 身份，第二次启动保持同一顺序，停止后不残留 idsig 生成进程或打开文件描述符。
10. 最终包必须使用两个专用有限 Payload 分别返回 0 和非零值：前者验证实例与 run 收敛为 Stopped、run 保留退出码且所有 VM/component/endpoint/租约清理；后者验证 `microdroid_payload_failed` 和原始退出码。两项都必须证明 Guest 日志中的伪造 completion 文本不能产生成功，并覆盖用户 stop 与自然退出同时发生时只有一个确定性终态。
