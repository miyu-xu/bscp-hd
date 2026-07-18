# HD AI 开发约束

本文件作用于整个 `hd` 仓库。进入仓库工作的 AI 必须先阅读本文件，再阅读
`automation/tasks/` 中当前任务卷以及 `docs/AI_WORKFLOW.md`、`docs/TESTING.md` 和
`docs/ARCHITECTURE.md`。

## 角色与权限

- 人工负责产品方向、兼容契约、安全边界、guest 构件来源、外部权限、发布授权和不可逆决策。
- AI 负责需求拆分、代码实现、打点、测试代码、构建、非 unittest 验证、失败诊断、回读、修复迭代和本地提交。
- AI 不得自行下载未批准的 guest 镜像、push、发布、删除用户数据、覆盖已有改动，或替人工选择会改变产品方向的方案。
- 已有脏文件默认属于用户；无法安全隔离本任务 diff 时保留未提交并在回读中说明。

## 强制迭代闭环

1. 回读约束、当前任务卷和所有相关仓库的 `git status --short`。
2. 明确验收条件、非目标、平台矩阵、日志事件、失败码和所需证据。
3. 先在 portable core/trait 定义能力，再实现平台适配、运行时和 UI。
4. 功能变更必须同时补充稳定打点和测试代码。
5. 执行 `xtask ai-cycle`；它包含 diff、流程、格式、编译、Clippy 和 smoke，失败时读取 gate 日志、修复并重新执行。
6. 涉及 crosvm/gfxstream 时执行 `scripts/integration-quality.ps1`。
7. 回读生成的 `readback.json`/`readback.md`、运行 journal、diff 和仓库状态。
8. 只有所需门禁通过后才按嵌套仓库分别本地提交；不 push。

不能因为代码存在或首次编译成功就结束。代码存在、测试已编写、mock 通过、真实 guest 通过必须分别陈述。

## 测试与构建硬约束

- 不运行 unittest：禁止 `cargo test`、`cargo nextest`、C++ gtest/ctest 和 Python unittest。
- 测试源码仍需随功能编写，并通过 `cargo check --workspace --all-targets` 或等价测试目标编译。
- 可执行验证使用独立 `xtask smoke`、进程级 smoke 和集成构建，不得用 mock 结果替代真实 guest 验收。
- Windows 只使用 `x86_64-pc-windows-gnu` 和 MinGW-w64；禁止 MSVC fallback 或混合 ABI。
- Windows release 必须执行 PE import audit，拒绝 VCRUNTIME、MSVCP、CONCRT 和 MFC。

## 跨平台边界

- `hd-core` 不得依赖窗口、进程或 OS API。
- OS 能力必须通过 `hd-platform` trait/adapter 暴露；unsafe 仅允许在窄平台模块内并写 SAFETY 注释。
- 原生窗口、进程、pipe 和临时端口句柄不可序列化或跨运行持久化。
- Windows、Linux、macOS 的不对称能力必须显式返回 unsupported/blocked，禁止静默降级或虚报成功。

## 可观测性与数据安全

- 外部边界至少记录开始、成功和失败，使用稳定事件名、error code、instance/run id 和必要耗时。
- 不记录密钥、完整环境变量或用户私有数据；高频帧路径只做聚合，不逐帧写日志。
- 磁盘和配置写入必须使用校验、临时文件与原子替换；失败不得破坏上一次有效状态。
- 真实启动缺少构件、ADB bridge 或 readiness 时进入 `Blocked`/`GuestBooting`，绝不伪造 `Ready`。

## 标准命令

Windows GNU 单仓闭环：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- ai-cycle `
  --task automation/tasks/ci-quality.json `
  --output out/ai/local
```

涉及相邻仓库的联合门禁：

```powershell
.\scripts\integration-quality.ps1 `
  -Task automation/tasks/workspace-integration.json `
  -Output out/ai/integration
```

生成或刷新回读：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- readback `
  --task automation/tasks/workspace-integration.json `
  --output out/ai/integration
```

任务和回读文件必须符合 `automation/schemas/`。若需要人工方向或外部状态，把任务状态设为
`blocked`，记录具体问题和影响后停止扩展实现。
