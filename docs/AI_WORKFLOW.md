# 完全 AI 开发与人工决策流程

## 角色边界

人工只负责方向和不可逆决策：产品优先级、guest 构件来源、协议兼容策略、安全边界、发布授权、外部系统权限与是否解除测试限制。AI 负责需求拆分、实现、构建、诊断、测试代码、非 unittest 自动验证、文档、回读、修复迭代和本地提交。

AI 不因“完全 AI 开发”获得额外权限：不得自行下载未批准 guest 镜像、push、发布、删除用户数据、覆盖用户改动或替人工选择会改变产品方向的方案。

## 每个增量的闭环

```text
约束/脏树回读
    ↓
写出行为与完成证据
    ↓
最小协议/架构改动
    ↓
实现 + 打点 + 测试代码
    ↓
format/check/clippy → smoke → 集成编译 → ABI 审计
    ↓                                  │
失败证据 ───────── 自动修复并重跑 ←───┘
    ↓
diff/status/运行证据回读
    ↓
按嵌套仓库本地提交（不 push）
```

AI 必须持续迭代到门禁通过，或遇到确实需要人工方向/外部状态的阻塞。不能因首次编译成功就停止，也不能用文档说明替代可执行验证。

## 仓库内强制入口

- `AGENTS.md` 是进入 HD 仓库的 AI 必读入口，固化角色、权限、测试禁令、跨平台边界和闭环命令。
- `automation/schemas/` 定义版本化任务卷、gate report 和回读格式；`automation/tasks/` 保存会话间交接状态。
- `xtask process-check` 校验上述资产、所有任务卷以及自动脚本未调用 unittest。
- `xtask quality` 首先执行 `process-check`、工作区与暂存区的 `git diff --check`，随后执行 format、all-targets check、Clippy 和独立 smoke。
- `xtask ai-cycle` 调用质量门，无论通过或失败都在 `out/ai/<task>/` 写 gate 日志、`hd-gates.json`、`readback.json` 和 `readback.md`。
- `scripts/integration-quality.ps1` 使用统一 MinGW 继续编译 crosvm 功能/测试目标和 gfxstream backend，再合并生成跨仓回读。

Windows 单仓自动闭环：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- ai-cycle `
  --task automation/tasks/ci-quality.json `
  --output out/ai/local
```

跨仓联合闭环：

```powershell
.\scripts\integration-quality.ps1 `
  -Task automation/tasks/workspace-integration.json `
  -Output out/ai/integration
```

AI 根据 gate log 修复后必须用同一任务卷重跑。脚本负责证据捕获和确定性门禁；代码修改仍由受本文件约束的 AI 完成，脚本不会获得额外写权限或自行改变产品方向。

## 任务卷

每个开发任务应具备：

- 目标行为和非目标；
- 影响的 schema/protocol/state；
- 平台矩阵与 MinGW ABI 影响；
- 日志事件、字段和失败码；
- 测试代码与可执行 smoke/集成场景；
- 回滚策略与数据迁移；
- 完成证据；
- 需要人工决定的项目。

任务卷使用 `automation/schemas/task.schema.json`，不得只存在于聊天上下文。状态为 `complete` 只表示当前任务验收完成；真实 guest 等未来里程碑阻塞要继续保留在 `blockers` 和独立证据状态中。

## 自动打点要求

每个外部边界至少记录开始、成功、失败：构件验证、磁盘复制、进程 spawn/exit、显示 attach/replace/rollback、ADB connect/action/install、IPC 协议拒绝。字段使用稳定名称，不写密钥或整份环境变量。高频渲染路径只聚合计数，不逐帧记录。

错误必须分为：输入/config、外部构件、平台、VM、ADB、IPC、journal 和状态机；对用户返回稳定 error code，对日志保留具体上下文。

## 自动提交策略

- 只有相关门禁通过后提交。
- `hd`、`external/crosvm`、`hardware/google/gfxstream` 分别提交，便于独立回滚。
- 使用显式文件列表暂存；提交前再次 `git diff --cached --check` 和 `git status --short`。
- 已存在的脏文件默认属于用户。若本次必须修改同一文件，报告重叠并只提交可确认属于本任务的完整 diff；无法隔离时不提交该仓库。
- 根 AOSP 工作树可能包含跨项目用户改动，默认保留未提交并在交付报告列出。
- 不自动 push、rebase、reset、清理或删除工作树。

## 人工决策点

以下情况暂停扩展实现并请求方向：

- 两种 guest/ADB/存储方案会改变兼容契约；
- 需要获取受限构件、凭据、签名或外部服务权限；
- 需要破坏性迁移或删除已有实例磁盘；
- 需要解除“不跑 unittest”约束；
- 真实测量与既定产品指标冲突；
- 用户已有改动与必要实现无法安全合并。

其余可逆、范围内的实现与诊断由 AI 自主推进。

## 回读报告格式

每轮交付只报告可证实结论：

1. 已实现的行为；
2. 实际执行的门禁与结果；
3. 未执行项及原因；
4. 当前阻塞和所需人工决策；
5. 本地提交及保留未提交文件；
6. 下一自动迭代入口。

“代码存在”“测试已编写”“mock 通过”“真实 guest 通过”是四种不同状态，禁止混写。

`xtask readback` 自动采集所需 gate、HD/crosvm/gfxstream/根工作树 HEAD 与 dirty 状态，并同时生成 JSON 和 Markdown。机器报告是事实底稿，最终交付说明不得删去失败 gate、缺失证据、用户遗留改动或阻塞。
