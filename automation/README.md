# AI 自动迭代资产

该目录把 `docs/AI_WORKFLOW.md` 的流程约定转换为机器可读输入和可验证输出。

- `schemas/task.schema.json`：任务卷格式；
- `schemas/gate-report.schema.json`：自动门禁结果格式；
- `schemas/readback.schema.json`：自动回读格式；
- `examples/`：两个 schema 的最小合法样例；
- `tasks/`：可执行任务卷，作为 AI 会话之间的稳定交接面；
- `out/ai/<task-id>/`：运行时生成的 gate 日志、gate report 和回读，不提交 Git。

任务状态不等于功能证据。`evidence_state` 必须分别描述代码、测试源码、mock 和真实 guest，
`xtask readback` 不会根据 UI 状态自动推断真实 guest 已通过。

## 新任务

1. 从 `examples/task.example.json` 复制到 `tasks/<task-id>.json`。
2. 填写目标、范围、约束、验收、所需 gate、决策、阻塞和下一迭代入口。
3. 开发期间使用 `planned`/`in_progress`；只有验收完成才改为 `complete`。
4. 若需要人工方向或外部输入，改为 `blocked` 并填写 `human_decisions_needed`。
5. 执行 `xtask ai-cycle`；跨仓任务执行 `scripts/integration-quality.ps1`。

`xtask process-check` 会校验所有已提交任务卷、schema、样例、必需流程文件和自动脚本中的
unittest 禁令。该检查也是 `xtask quality` 的第一道门禁。
