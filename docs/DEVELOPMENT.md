# HD 开发与构建

## 工具链

Windows 唯一支持目标是 `x86_64-pc-windows-gnu`：Rust stable、MinGW-w64 `gcc/g++/ar/mingw32-make/objdump`，CMake C/C++ 项目使用 `MinGW Makefiles`。禁止 MSVC fallback，也禁止 GNU Rust exe 链接 MSVC C/C++ 产物。

`.cargo/config.toml` 固定 GNU linker/ar，`build.bat` 拒绝其他 Windows target，`xtask pe-audit` 拒绝 `VCRUNTIME`、`MSVCP`、`CONCRT` 和 `MFC` import。默认搜索：

```text
C:\workspace\mingw64
C:\tools\mingw64
C:\msys64\mingw64
C:\mingw-w64\mingw64
```

自定义位置：

```bat
set "MINGW_PATH=D:\toolchains\mingw64"
```

Linux/macOS 使用各自原生 Rust/C 工具链；portable contract 相同，但目标平台的虚拟化、GPU、IPC、文件安全和进程身份必须由原生 runner 验证。

## 构建入口

Host 质量门（不运行 unittest）：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- quality
```

Windows workspace release + PE audit：

```bat
build.bat
```

根仓库统一发布：

```bat
build_all.bat
```

根构建依次处理 binder-rpc、可选 gfxstream/ANGLE、virtmgr/vm/crosvm 和 HD，发布到 `out\dist\windows\bin`。HD 运行时必须包含：`hd.exe`、`hdctl.exe`、`hd-host.exe`、`hd-worker.exe`、`hd-device-sim.exe`。启用本仓库 gfxstream/ANGLE：

```bat
set ENABLE_GFXSTREAM_ANGLE=1
build_all.bat
```

原生 Linux/macOS runner：

```bash
cargo run -p xtask -- quality
cargo build --workspace --release
```

在非原生主机执行 `cargo check --target ...` 只能证明 Rust 编译；缺少 linker/SDK 时记录为环境阻塞，不能代替原生运行证据。

## 变更方法

1. 阅读 `AGENTS.md`、当前任务卷、AI workflow/architecture/testing，并回读所有相关仓库状态。
2. 把行为、schema/protocol/state、稳定错误码、事件和完成证据写入任务卷。
3. 先修改 `hd-core` portable contract；OS 差异进入 `hd-platform`，运行编排进入 `hd-runtime`，最后接 UI/CLI。
4. 同步编写测试源码和独立黑盒场景；测试目标必须编译，但不运行 unittest。
5. 运行相同任务卷的 `xtask ai-cycle`；失败即回读 log、修复、重跑。
6. 触碰 crosvm/gfxstream/根构建时运行 `integration-quality.ps1`。
7. 回读 run journal、诊断、gate/readback、diff/status 和 PE ABI；不以代码存在代替运行证据。
8. 只有用户要求时按嵌套仓库显式暂存并本地提交；不 push，不处理无关脏文件。

## 代码边界

- `hd-core` 只包含可序列化数据、验证和状态，不引入窗口/进程/OS crate。
- unsafe 仅在窄平台模块，逐块写 SAFETY 依据；原生 handle 不序列化。
- Windows/Unix 私有文件在创建时就设置 owner ACL/mode；敏感读取 no-follow，更新使用同目录临时文件、fsync/flush 和原子替换。
- 外部命令不得接受用户自由拼接参数；从类型化 V2 配置和已验证 bundle 生成。
- Host/Worker/HTTP/IPC 边界必须有大小、超时、身份和版本检查。
- 不能真实热应用的配置返回 restart-required；不能提供的能力返回 Blocked/unsupported，不静默降级。
- 高风险租约只有在进程与 endpoint 清理得到证明后释放。
- frame 路径不阻塞、不逐帧打日志、不暴露 CPU 像素回退。

## 依赖和供应链

- Rust 依赖固定在 workspace `Cargo.toml` 和 `Cargo.lock`，新增依赖需评估三平台、许可、体积和原生 ABI。
- 运行时不下载 Guest、ADB、crosvm 或 device component。
- Artifact store 只接受受信 Ed25519 签名、内容寻址、READY、逐文件大小/SHA-256 均通过的 V2 bundle。
- 发布认证只由授权私钥在八类原始证据全部存在后签发；私钥不进入 data root、日志、诊断或仓库。
