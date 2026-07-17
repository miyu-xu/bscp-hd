# 完全 AI 开发、验证与回读闭环

## 角色边界

人工只负责方向决策：产品优先级、Guest/组件来源、信任根、兼容策略、安全边界、不可逆迁移、外部权限和发布授权。AI 负责需求拆分、架构/协议、实现、打点、测试源码、构建、非 unittest 黑盒验证、失败诊断、修复迭代、文档、证据回读和经授权的本地提交。

“AI 为主”不扩大权限。AI 不下载未批准镜像，不创建/使用未知凭据，不发布或 push，不删除用户数据，不 reset/rebase/清理脏树，也不替人工做会改变产品契约的选择。

## 仓库内固化资产

流程不是聊天约定，而由以下文件和程序共同强制：

- `AGENTS.md`：角色、权限、MinGW、跨平台、数据安全和不运行 unittest 的最高层约束；
- `automation/schemas/task.schema.json`：V2 任务卷，包括目标、范围、验收、gate、证据分层、方向决策、阻塞和下一迭代；
- `automation/schemas/gate-report.schema.json`：每个可执行 gate 的命令、时间、退出码、日志和结果；
- `automation/schemas/readback.schema.json`：gate 汇总、仓库 HEAD/dirty、证据状态、阻塞和结论；
- `automation/tasks/*.json`：可跨会话继续的事实状态；
- `xtask process-check`：逐个校验上述 schema/任务/样例/必需文档，并扫描自动脚本中的 unittest 调用；
- `xtask quality`：流程、diff、格式、all-targets、Clippy、build 和独立 smoke；
- `xtask ai-cycle`：执行质量门，即使失败也生成 gate report 与 readback；
- `xtask readback`：合并已有 gate，回读 HD/crosvm/gfxstream/根仓库状态；
- `scripts/integration-quality.ps1`：在同一 MinGW ABI 下执行跨仓编译和根发布验证；
- `.github/workflows/ci.yml`：CI 调用同一入口，并在失败时仍上传回读证据。

修改这些流程资产本身也必须通过 `process-check`，不能只在文档中声称已经固化。

## 每轮确定性闭环

```text
AGENTS + 任务卷 + 脏树 + 上轮 readback
                    │
                    ▼
       行为/错误码/事件/完成证据
                    │
                    ▼
 portable V2 contract → 平台 adapter → runtime → UI/CLI
                    │
                    ▼
      测试源码 + 黑盒场景 + 诊断/证据输出
                    │
                    ▼
 process/diff/fmt/check/clippy/build/smoke/integration/runner
                    │
          失败日志 ─┴─→ AI 修复并按同一任务卷重跑
                    │
                    ▼
 journal + gate + readback + diff/status/ABI 回读
                    │
                    ▼
        全部证据通过，或精确外部阻塞
```

AI 不得在首次编译、单个 smoke 或 UI 可见时结束。只要存在范围内且安全的修复，就继续迭代；只有确实缺少人工方向、受限制品/签名、外部权限或目标硬件时，才保留 `in_progress`/外部 blocker 并明确所需输入。

## 任务卷规则

每项工作从 `automation/examples/task.example.json` 建立 `automation/tasks/<task-id>.json`，必须包含：

- 一个可验证 objective，不使用“完善一下”等开放措辞；
- 精确 repositories/owned/excluded paths，防止夹带脏树；
- schema/protocol/state/数据迁移影响；
- Windows/Linux/macOS 与 ABI 影响；
- 每项 acceptance 的 gate 或 artifact 证据；
- 稳定事件名、错误码、失败注入和安全边界；
- 人工已经作出的 direction decisions；
- external blocker、影响和解除条件；
- `code_present`、`tests_authored`、`contract_smoke_verified`、`real_guest_verified` 四个独立布尔值；
- 可直接运行的 `next_iteration`。

任务状态不能伪造证据。Host 子目标通过时可更新对应 acceptance/evidence，但原子交付存在 real-guest/zero-copy/device 缺口时，主任务仍保持 `in_progress`。只有 required gate 全部通过且没有所需工作时才使用 `complete`。

## 打点与失败语义

每个外部边界必须记录 started/succeeded/failed：能力/制品验证、迁移、租约、磁盘、Worker/crosvm spawn/exit、frame 握手、ADB connect/readiness/action/install、HTTP/IPC 拒绝、诊断打包和清理。事件包含稳定名称、trace/instance/run/operation id、耗时与错误码；不写 bearer、worker secret、签名私钥、完整环境或用户内容。

高频 frame 路径只保留聚合 metrics，不逐帧写日志。错误按 config、artifact/trust、capability/certification、platform、store/lease、worker/VM、frame、ADB/device、HTTP/IPC、journal/diagnostic 分类；API 返回稳定 code，结构化日志保留具体上下文。

启动或停止失败必须得到一个可回读终态。若子进程/endpoint 清理未被证明，设置 `cleanup_pending`，保留精确身份和租约；不能为了界面整洁把状态写成 Stopped。

## 标准命令

Windows 单仓：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- ai-cycle `
  --task automation/tasks/hd-p0-v2.json `
  --output out/ai/hd-p0-v2
```

跨仓：

```powershell
.\scripts\integration-quality.ps1 `
  -Task automation/tasks/workspace-integration.json `
  -Output out/ai/integration
```

已有外部 runner gate 时重新回读：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- readback `
  --task automation/tasks/hd-p0-v2.json `
  --output out/ai/hd-p0-v2 `
  --gate-report <gate-report.json>
```

AI 从 gate log 定位失败，修复后使用相同任务卷和输出语义重跑。不得手工编辑 gate report 把失败改为通过。

## 变更与提交回读

- 进入工作前分别读取 HD、crosvm、gfxstream 和根仓库状态；已有改动默认属于用户。
- 编辑时只触碰任务卷 owned path；与用户改动重叠时逐块审计，无法隔离就不提交。
- 完成验证后回读 `git diff --check`、相关 diff、`git status --short`、生成物清单和 run journal。
- 若用户要求本地提交，按嵌套仓库分别使用显式文件列表暂存，再执行 cached diff check；不 push。
- 不自动删除 out/target、诊断、实例磁盘或用户文件；临时 smoke 只使用自动清理的独立目录。

## 需要人工决策的边界

只有以下问题暂停方向性实现：两种方案会改变 Guest/ADB/frame/存储兼容契约；需要受限制品、信任根、签名私钥或外部服务权限；需要破坏性迁移/删除；需要解除 unittest 禁令；真实测量与已批准指标冲突；必要改动无法与用户脏树安全隔离。

缺少签名 bundle 或专用 runner 不妨碍继续修复 Host 范围内的问题，但阻止发布结论和认证签发。最终报告必须分别列出已实现行为、实际门禁、未执行项、外部阻塞、工作树状态和下一自动入口。
