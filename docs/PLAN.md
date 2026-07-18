# HD 实施计划与完成定义

## 产品目标

HD 提供常见 Android 模拟器的桌面管理体验，同时把虚拟化、显示、guest 构件和宿主平台细节放在明确的适配层中。首个落地目标是 Windows + MinGW + crosvm + gfxstream；可移植核心必须持续在 Linux/macOS 编译，平台能力不能散落到 UI 或协议层。

## 里程碑

### M0：工程与流程基线（已完成）

- Rust workspace、独立 Git 子仓库、MinGW 构建和根 `build_all.bat` 接入。
- 配置 schema、控制协议、GPU 遥测协议和实例状态机。
- 结构化应用日志与每次运行的 manifest/events/result。
- 所有测试目标只编译；独立 `xtask smoke` 执行 mock 与阻塞启动流程，不调用 unittest。
- AI 开发、回读、差异审计、提交和人工决策规则文档化。

完成证据：格式检查、GNU `cargo check --all-targets`、Clippy、烟测、release 链接、PE 审计通过。

### M1：桌面多实例与常规控制（已完成阶段 0）

- 实例列表、创建、保存、启动、停止、删除和诊断。
- 主页、最近、返回、旋转、电源、音量与 APK 安装入口。
- CPU、内存、显示、VSync、FPS、ADB、构件路径和额外 kernel 参数设置。
- Mock 后端在无 Android 构件时完成确定性流程验收。

真实 guest 控制只有在 ADB bridge 建立且状态达到 `Ready` 后才算完成，当前不得把阶段 0 UI 入口等同于真实设备验收。

### M2：Windows 嵌入显示链（代码完成，待真实 guest 回读）

- HD 创建由 UI 所有的容器 HWND。
- crosvm 接收 launch-only `parent-window-handle`，gpu_display 使用 `WS_CHILD`。
- crosvm `gpu replace-display` 保持 scanout id，支持动态显示参数。
- gfxstream VSync on 强制 FIFO；off 按 IMMEDIATE、MAILBOX、FIFO 降级并记录告警。
- gfxstream 仅统计 `vkQueuePresentKHR` 成功帧，按秒上报 FPS。

完成定义：真实 guest 连续启动/停止 100 次无孤儿窗口；窗口缩放、最小化、切换实例和旋转无句柄泄漏；VSync/FPS 与外部测量一致。

### M3：cuttlefish guest 启动闭环（当前阻塞）

需要人工方向决策并提供或确认：

1. kernel/initrd/rootfs/fstab 的正式来源、版本和 SHA-256 清单；
2. Windows crosvm 的 guest block/boot 参数最终契约；
3. ADB 采用 TCP、vsock 或代理桥接的正式方式及 guest service port；
4. “已启动”的唯一就绪条件，例如 boot-completed + package manager + surface；
5. system/vendor 独立镜像是否作为额外磁盘挂载。

实现内容：端口租约、ADB connect/retry/root、启动超时、进程退出监控、真实状态推进、错误分类和诊断采集。

完成定义：干净数据目录完成真实 Android 首启；连续 20 次启动成功率 100%；Home/Recent/Back/APK/旋转均在 guest 回读确认；失败运行具备完整证据包。

### M4：跨平台宿主实现

- Linux：明确 X11 与 Wayland 子表面/外部窗口策略，并接入平台 VM 后端。
- macOS：AppKit view 与 Hypervisor/crosvm 显示策略。
- 保持 `hd-core` 无平台 API，平台句柄只存在于 `hd-platform` 的进程内 lease。

完成定义：Linux、macOS 可移植检查持续通过；每个平台有独立 smoke/集成证据，不以 Windows 条件分支冒充实现。

### M5：产品化与长期质量

- 实例克隆、快照策略、升级迁移、磁盘配额与数据回收。
- 崩溃恢复、运行锁、单 supervisor 发现/拉起策略。
- 性能基线、长稳、输入延迟、GPU 兼容矩阵和可观测性面板。
- 签名、发布清单、SBOM、依赖许可与供应链扫描。

## 优先级与退出条件

任何迭代按以下顺序处理：数据安全与状态正确性 > 可诊断性 > 功能正确性 > 性能 > 体验。若真实 guest 不可用，AI 继续完善可验证的宿主契约、mock 和构建链，但必须把真实验收标为阻塞，不能用 mock 结果替代。
