# 测试、质量与回读

## 仓库执行约束

当前仓库明确要求“不跑 unittest”。因此质量门禁不会调用 Rust `cargo test`、C++ gtest 或 Python unittest。测试代码仍应随功能编写，并通过 `cargo check --workspace --all-targets` 编译；可执行流程由独立 `xtask smoke` 验证。若未来允许执行 unittest，必须由人工明确变更该约束后再开启。

## 自动门禁

Windows：

```powershell
cd hd
.\scripts\quality.ps1 -WindowsGnu
```

门禁依次执行：

1. `cargo fmt --all -- --check`；
2. GNU target 的 `cargo check --workspace --all-targets`，包括测试代码编译；
3. GNU target 的 `cargo clippy --workspace --all-targets -- -D warnings`；
4. `xtask smoke`；
5. 可选 `build.bat` release 链接和 PE import audit。

`xtask smoke` 不使用测试 harness，当前覆盖：

- 创建持久化实例；
- mock start 逐步达到 Ready；
- Home/Recent/Back/Rotate；
- mock APK 安装；
- 显示配置事务；
- stop 写入终态；
- 缺少真实构件时进入 Blocked；
- Blocked 可正常 stop；
- 两次运行均存在 manifest/events/result。

## 集成编译矩阵

| 层 | Windows GNU 门禁 | Linux/macOS 门禁 | 运行回读 |
|---|---|---|---|
| HD Rust workspace | check/clippy/all-targets | check/clippy/all-targets | mock smoke |
| crosvm vm_control | GNU cargo check | 后续 CI | 命令协议待真实 VM |
| crosvm gpu_display/devices/crosvm | 指定 features 的 GNU cargo check | 后续 CI | 子 HWND 待真实 guest |
| gfxstream Vulkan backend | CMake MinGW Release build | 后续 CI | VSync/FPS 待真实 present |
| 发布目录 | release build + objdump | 平台包后续 | 启动/UI 人工方向验收 |

## 回读清单

任何“完成”结论都必须同时回读：

- 命令退出码和 warning；
- `git diff --check`；
- 各嵌套仓库 `git status --short`，确认没有夹带用户文件；
- smoke 的运行目录；
- `manifest.json` 中的命令、环境、构件指纹和 toolchain；
- `events.jsonl` 状态顺序、事件序号和失败原因；
- `result.json` 最终状态与退出码；
- Windows PE imports，不含 MSVC runtime。

只看到 UI 按钮、状态字段或 mock Ready 不等于真实功能通过。

## 真实 Android 验收（M3 解阻后）

自动场景：

1. 空数据目录冷启动并等待唯一 readiness probe；
2. 20 次启动/停止循环；
3. 两个实例并行启动、独立磁盘与独立 ADB serial；
4. Home/Recent/Back/Power/Volume 的 guest 回读；
5. APK 安装后用 package manager 验证；
6. 横竖屏显示尺寸和 Android rotation 一致；失败注入验证回滚；
7. VSync on/off present mode 与 FPS 聚合回读；
8. crosvm 异常退出、ADB 超时、坏 checksum、磁盘不足等失败注入；
9. 100 次窗口创建/销毁和实例切换，检查 handle/process/task 泄漏；
10. 2 小时长稳与日志体积上限。

## 质量判定

- P0：数据损坏、越权 IPC、孤儿 VM、句柄复用、状态虚报；不得发布。
- P1：核心动作、显示事务、启动/停止失败；不得进入下一里程碑。
- P2：诊断、兼容性或体验问题；必须有 issue、证据和明确降级。
- 性能回归：CPU/内存、启动耗时、present FPS 和日志量超过已批准基线时阻断。
