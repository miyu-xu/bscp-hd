# AI 自动迭代资产

该目录把 `docs/AI_WORKFLOW.md` 的流程约定转换为机器可读输入和可验证输出。

- `schemas/task.schema.json`：任务卷格式；
- `schemas/gate-report.schema.json`：自动门禁结果格式；
- `schemas/readback.schema.json`：自动回读格式；
- `examples/`：两个 schema 的最小合法样例；
- `tasks/`：可执行任务卷，作为 AI 会话之间的稳定交接面；
- `out/ai/<task-id>/`：运行时生成的 gate 日志、gate report 和回读，不提交 Git。

任务状态不等于功能证据。V2 `evidence_state` 必须分别描述代码、测试源码、contract
smoke 和真实 Guest；`xtask readback` 不会根据 UI 状态、Blocked 行为或协议存在自动推断
真实 Guest 已通过。

## 新任务

1. 从 `examples/task.example.json` 复制到 `tasks/<task-id>.json`。
2. 填写目标、范围、约束、验收、所需 gate、决策、阻塞和下一迭代入口。
3. 开发期间使用 `planned`/`in_progress`；只有 required gate 全部通过才改为 `complete`。
4. 外部制品/runner 缺失写入 `blockers` 并保持可继续工作的主任务为 `in_progress`；只有确实
   无法继续且需要人工方向时才使用 `blocked` 和 `human_decisions_needed`。
5. 执行 `xtask ai-cycle`；跨仓任务执行 `scripts/integration-quality.ps1`。

`xtask process-check` 会校验所有已提交任务卷、schema、样例、必需流程文件和自动脚本中的
unittest 禁令。该检查也是 `xtask quality` 的第一道门禁。
