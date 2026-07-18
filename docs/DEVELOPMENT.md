# 开发与构建

## Windows 工具链

Windows 只支持：

- Rust stable，目标 `x86_64-pc-windows-gnu`；
- MinGW-w64 的 `gcc.exe`、`g++.exe`、`ar.exe`、`mingw32-make.exe`、`objdump.exe`；
- CMake 的 `MinGW Makefiles` generator。

不允许 MSVC fallback，也不允许 GNU Rust 二进制链接 MSVC C/C++ 产物。`build.bat` 会拒绝其他 Windows target，`.cargo/config.toml` 对 GNU target 固定 gcc/ar，PE audit 会拒绝 `VCRUNTIME`、`MSVCP`、`CONCRT` 和 `MFC` imports。

默认查找 `C:\workspace\mingw64`、`C:\tools\mingw64`、`C:\msys64\mingw64`、`C:\mingw-w64\mingw64`。其他位置使用：

```bat
set "MINGW_PATH=D:\toolchains\mingw64"
```

## 构建命令

HD release + PE audit：

```bat
cd hd
build.bat
```

项目统一流水线会依次构建 binder-rpc、可选 gfxstream、virtmgr/vm/crosvm、HD，并发布到 `out\dist\windows`：

```bat
build_all.bat
```

若要构建本仓库 gfxstream/ANGLE 路径：

```bat
set ENABLE_GFXSTREAM_ANGLE=1
build_all.bat
```

Linux/macOS 可移植编译：

```bash
cd hd
cargo run -p xtask -- check-portable
```

这只证明 Rust 分层和宿主代码可编译，不代表 crosvm/display guest 已在对应平台完成运行验收。

## 日常变更流程

1. 回读根仓库、`hd`、`external/crosvm`、`hardware/google/gfxstream` 的状态，标记用户已有改动。
2. 为单一可验收行为更新配置/协议，保持 schema version 和向后策略明确。
3. 在 portable core/trait 中定义能力，再写平台实现和 UI 入口。
4. 增加测试用例；按仓库约束只编译测试目标，不执行 unittest。
5. 运行质量、独立 smoke、相关 crosvm/gfxstream MinGW 编译和 diff audit。
6. 回读 manifest/events/result，确认行为与文档一致。
7. 只暂存本次拥有的文件；嵌套仓库分别提交，不 push。

任务开始前从 `automation/examples/task.example.json` 建立 `automation/tasks/<task-id>.json`。开发完成后优先执行：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- ai-cycle `
  --task automation/tasks/<task-id>.json `
  --output out/ai/<task-id>
```

相邻仓库发生变化时执行 `scripts/integration-quality.ps1`。具体 gate、仓库状态和证据分层由 `xtask readback` 生成，AI 必须回读该结果后才能给出完成结论。

推荐命令：

```powershell
.\scripts\quality.ps1 -WindowsGnu
git diff --check
git status --short
```

`quality.ps1` 执行 format check、GNU all-target check、Clippy 和非 unittest smoke。它不会调用 `cargo test`。

## 依赖与可重复性

- 所有 Rust 版本写入 workspace `Cargo.toml` 并提交 `Cargo.lock`。
- HD 不在运行时下载 kernel、镜像、ADB 或 crosvm。
- 外部构件由配置指定，启动前验证非空 regular file，并记录实际 SHA-256；配置可要求预期 SHA-256。
- Windows 发布以 PE import audit 作为 ABI 完成条件，不能只看 `cargo build` 返回码。
- 添加依赖前说明跨平台影响、许可、二进制体积与供应链来源。

## 代码约束

- `hd-core` 不得引入窗口/进程/OS API。
- 所有 IPC/日志 wire data 都要版本化且可序列化。
- 原生句柄不跨进程持久化；unsafe 只能在窄平台模块内，并写 SAFETY 注释。
- 不在帧路径打印日志或进行阻塞 IPC。
- 启动失败必须进入 `Failed`/`Blocked` 并落 result；不能停留在中间状态。
- 动态设置若不能真实热应用，必须返回需重启，不能只改配置后声称成功。
