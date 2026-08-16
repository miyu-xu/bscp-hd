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

根构建依次处理 binder-rpc、可选 gfxstream/ANGLE、virtmgr/vm/crosvm、HD React/Vite UI 和 Rust workspace，发布到 `out\dist\windows\bin`。HD 发布十四个 Windows 运行时；`hd.exe` 合并 Manager/Player，使用 winit 桌面壳、三个互相隔离的 Wry/WebView2 表面（顶栏、侧栏、页面）与 `NativeDisplayHost`，并把 `web/dist` 发布为 `bin/ui`。Windows 顶栏由 React/WebView 实现，Win32 只保留根窗口生命周期和固定宽高比控制；macOS 对应标题栏由 AppKit 实现，两端共用同一动作与状态契约。Casimir 和 RootCanal adapter 分别依赖清单固定的 AOSP 源码，frame producer 依赖同次构建的本仓库 gfxstream backend。

```bat
set ENABLE_GFXSTREAM_ANGLE=1
build_all.bat
```

日常修改使用按包快速检查，不运行 capability probe、bundle 逐文件校验或 Release 链接：

```powershell
.\scripts\dev-fast.ps1 -Package hd-runtime,hdctl
.\scripts\dev-fast.ps1 -Package hd-ui
```

`dev-fast.ps1` 只执行所选 package 的 fmt/check/build，不做 capability probe 或 bundle 逐文件验证；选择 `hd-ui` 时额外执行增量 Vite 构建。需要发布包含大 Guest rootfs 的正式 bundle 时，先构建并直接使用 `target/<target>/release/xtask`。

只有需要部署变更的可执行文件时增加 `-Release`；该脚本会单独报告 fmt、check 和 build 耗时。真实 Guest 的快速迭代可设置 `HD_DEV_FAST_ARTIFACTS=1` 跳过重复 bundle 内容校验，但它不会启用显示 copy fallback，也不能替代发布前的完整签名 bundle、capability、PE 和平台 gate。`windows-real-guest.ps1` 与 `windows-fault-injection.ps1` 默认把该变量传给其可能拉起的新 Host，并在证据中写入 `dev_fast_artifacts=true` 与 `capability_and_bundle_revalidation=skipped-by-design`；传 `-DevFastArtifacts $false` 会明确清除该环境变量并在真实 Guest 证据中写入 `performed-by-host`，切回正式 certification 路径。


`HD_DEV_GUEST_BUNDLE_ROOT` 和 `HD_DEV_HOST_TOOLS_ROOT` 是清单相对路径的替代根目录，不是搜索目录：运行时只做 `override-root + manifest.relative_path`，不会递归查找或从旧 build 目录猜测同名文件。`HD_DEV_GUEST_ROOTFS` 可单独覆盖 rootfs role；`hd.exe` 在 direct-linux 目录中优先选择 `aggregate_android.sparse.img`，找不到时才兼容旧 `aggregate_android.img`。Android sparse 源盘在第一次启动时展开为每实例可写的文件系统 sparse raw overlay，后续启动直接复用；不会把 crosvm 的只读 Android sparse backend 误用于 userdata 写入。快速模式仍会检查每个解析结果是存在的普通文件，只跳过内容 hash。若 Host 清单保留了去掉盘符后的绝对布局（例如 `workspace\...` 与 `Users\...`），Windows 的替代根应显式设为对应盘根 `C:\`；run manifest 的 `launch.executable` 和进程 `ExecutablePath` 是确认实际选中产物的准确信息。

固定产物完成后使用 `xtask publish-bundle` 生成签名且内容寻址的 Guest/Host bundle。该命令拒绝绝对路径、父目录、symlink、空文件、重复 role/path/capability 和未受信 signer，并在发布前用生产 `ArtifactResolver` 复验 manifest、READY、签名及逐文件 hash。

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
