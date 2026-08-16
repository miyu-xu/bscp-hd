# HD 质量、证据与回读

## 执行约束

仓库明确要求不运行 unittest。因此禁止 `cargo test`、`cargo nextest`、C++ gtest/ctest 和 Python unittest。测试源码仍随功能维护，并由 `cargo check --workspace --all-targets` 和 Clippy 编译；可执行行为由独立黑盒 smoke、进程场景、集成构建和专用 real-guest runner 验证。只有人工明确修改仓库约束后才能启用测试 harness。

## Host 质量门

Windows：

Windows 原生直显的补充硬门：run manifest 除 `HD_FRAME_REQUIRED=0` 外还必须设置 `HD_NATIVE_ZERO_COPY_REQUIRED=1`，且不得设置 `HD_FRAME_BROKER_V2`。crosvm 在该模式下不得分配或暴露 CPU framebuffer；正常显示只允许 gfxstream Vulkan present，只有诊断录屏可以产生有界 readback。

```powershell
cd hd
cargo run --target x86_64-pc-windows-gnu -p xtask -- quality
```

`xtask quality` 固定执行：

1. `process-check`：校验 AGENTS、V2 schema、样例、所有任务卷和脚本中的 unittest 禁令；
2. 工作区与暂存区 `git diff --check`；
3. 对 HD workspace 的十五个自有 package 逐项执行 `cargo fmt --package ... -- --check`；上游 AOSP RootCanal/Casimir 源由各自仓库格式规则管理，不因 Windows checkout 的换行策略被批量改写；
4. MinGW `cargo check --workspace --all-targets`；
5. MinGW `cargo clippy --workspace --all-targets -- -D warnings`；
6. MinGW `cargo build --workspace --bins`；
7. 独立 `xtask smoke`。

`xtask smoke` 当前以临时私有 data root 执行真实文件、数据库、HTTP 和进程边界，覆盖：

- V1 文件迁移到 V2、原始 backup 和 redb 结果；
- 单 data root 的 Host 独占锁；
- Host runtime descriptor、健康身份、可执行文件 SHA-256 和客户端连接；
- 跨版本 Host 升级：无活动实例时自动接管且无进程泄漏；真实 Microdroid Ready 时保留旧 Host/Worker/Guest，显式停止后才切换并复验当前摘要；
- 实例创建、列表、持久化与幂等 start operation；
- 缺失认证时 start 必须失败并进入 `Blocked`，不能产生 Ready；
- 无 bearer、恶意 Origin 和错误 Host 的 HTTP 拒绝；
- 两实例 CPU/内存/CID/ADB/GPU/磁盘/Worker/frame generation 租约隔离、generation 单调、audit 及完全释放；新建存储按完整容量准入，已有且尺寸兼容的实例存储重启仅要求 1 GiB 运行余量；
- `android-artifact-selection-smoke` 固定多 target AOSP 总根的 Host 架构选择：x86_64 只能选择 `vsoc_x86_64`，arm64 只能选择 `vsoc_arm64_only`/`vsoc_arm64`，未知架构不得猜测，任何平台都没有跨架构 fallback；可附加实际制品根参数复验四个 direct-linux 必需输入；
- `microdroid-exit-contract-smoke` 只信任宿主 `vm` stdout/stderr 与实际进程退出码，覆盖自然成功、Payload 非零退出码保留、Guest 日志伪造隔离、wrapper 非零、非 Shutdown、畸形/溢出和重复 callback；
- `runtime-storage-smoke` 对已完成 run 的超限日志自动保留 16 MiB 尾部，按每实例最多 20 轮和 2 GiB 总预算清理最旧记录，同时至少保留最近 5 轮；完成 run 的可再生 patched initrd 必须删除，没有 `result.json` 的活动 run 必须拒绝该清理并保持原文件；
- Blocked 实例 stop、租约完全释放和实例删除；
- APK 流式上传、ZIP/APK 结构校验与诊断 archive/hash 精确回读；
- Host shutdown 后 descriptor 清理；
- Windows managed child 以挂起状态启动、纳入 kill-on-close Job Object 后恢复；关闭 containment 必须按 PID/启动标记终止父子进程树；
- Windows Vulkan Win32 frame producer 的真实驱动 probe 必须证明 memory export、same-adapter、显式同步和无 copy/readback 回退；
- 真正 detached `hd-worker` 进程、PID/启动标记/nonce、实例 OS lock、重复 Worker 拒绝、正确 secret Ping、错误 secret 拒绝、descriptor 删除后认证重建、认证 Shutdown、进程退出和 descriptor 清理；
- 固定 Guest profile 的设备角色、serial/virtio-console、三个稳定 network device（保留 Cuttlefish Wi-Fi/mobile 槽位并以独立 eth2 提供 Ethernet 上行）和 virtio-snd 启动参数；正式设备 component 的精确最小角色授权、launch/ready/process 身份、错误 bearer 拒绝、正确 bearer Ping、响应身份和受管终止；UWB adapter 必须完成真实双向 Guest 命名管道交换。modem 必须经 Guest RIL host-vsock 9697 完成无 reset 风暴的确定性 AT baseline，并在 Guest 尚未发送查询时先按顺序主动发布 `+CREG/+CGREG/+CEREG` 注销、LTE 重新注册和 13 字段 `+CSQ`，随后显式 `AT+CSQ`、`AT+COPS?`、`AT+CREG?` 查询仍须返回同一状态；Windows 命名管道和 macOS Unix stream 必须使用同一断言。其余响应绑定请求 SHA-256；保留的 network/audio/camera adapter profile 必须 probe `formal=false` 并拒绝启动，不能把没有数据面的 stub 计入能力；越界 typed action 保持稳定 `action_invalid` HTTP 边界；
- Windows 音频启动参数必须区分虚拟声卡与宿主隐私授权：默认或关闭时使用 WASAPI playback 且 `num_input_devices=0`，按实例显式选择系统默认麦克风后才允许 `capture=true` 和一个 input PCM；关闭音频设备必须同时清除宿主麦克风选择。macOS 必须使用内建 CoreAudio 后端并保持 `capture=false`、`num_input_devices=0`，以真实 Android `tinyplay` 多缓冲成功、crosvm 存活且无 `FetchBuffer`/TimerAsync EINVAL 为 Host 播放证据；在 TCC 和真实采集闭环完成前能力与 UI 必须继续阻断宿主麦克风。未来 macOS Host 麦克风 gate 必须同时证明稳定签名身份的 TCC allow、拒绝/撤销边界、Guest WAV 时长和格式、PCM 的 RMS/peak/非零样本、关闭后 input PCM 消失以及实例/crosvm 仍 Ready；只有帧数和 WAV 容器而 PCM 全零必须失败；
- 临时 Ed25519 trust/signing key 下的 bundle staging、逐文件 hash、签名、READY、自验证和内容寻址原子发布。

这个 smoke 被命名为 contract/process smoke，只证明 Host 控制面和硬阻断正确，不生成 Android 运行成功证据。

标题栏同步另由 `hd-titlebar-contract-smoke` 做快速门禁：Windows 使用与侧栏/内容隔离的独立 WebView 顶栏，macOS 使用 AppKit 原生 accessory；两端必须保持 11 项标题栏动作（1 项全局侧栏动作和 10 项 Android Player 动作）的顺序、精确命令绑定、共享侧栏开合反馈、播放器页可见性、readiness/录屏/FPS 状态，以及最小化/展开/关闭的同等窗口语义；主/副屏选择不占用标题栏。Windows 顶栏点击不得进入全局 busy 广播、不得在几何未变化时重复调整 WebView/Android 子窗口，并在最早的 `pointerdown` 阶段阻止默认焦点切换（后续 `mousedown` 继续防守），避免 WebView2/DWM 合成中间黑帧。切换到设置、设备或诊断页时，两端保留全局侧栏按钮，同时隐藏 10 项 Android Player 动作和 FPS；返回播放器页再按共享状态恢复。标题栏的“安装 APK”在两端都必须每次打开平台文件选择器并在选择后立即安装，不能在 Windows 静默重装上一次选择的文件；设置页的显式路径与“安装已选 APK”是另一条操作。macOS 原生运行该 smoke 时会直接读取 AppKit 控件表；Windows 门禁读取独立 WebView 顶栏的 DOM 命令绑定、稳定状态去重、焦点与原生显示隔离契约。`smoke-webview-disjoint.ps1 -VerifyTitlebarFocus` 在交互桌面物理点击前后读取焦点；非交互桌面必须明确记录 `physical_input_pending`，不能伪造通过。

同一门禁还读取 Windows crosvm 源契约：显式 windowed 分辨率必须逐像素成为固定 Guest scanout，不能再次套用 Host 全屏软限制；Host viewport 只负责按比例显示。显式 `--virtio-snd` 的声卡数量及参数必须逐实例保留，不能把用户选择的零输入或系统默认麦克风静默重建为统一零输入设备。crosvm 非沙箱设备子进程必须独立使用 `CREATE_NO_WINDOW`，不能依赖父进程恰好没有控制台。Windows host-vsock 命名管道的重叠读失败必须移除连接并向 Guest 发送 `VIRTIO_VSOCK_OP_RST`，不能留下 Guest 侧永久等待的半开 ADB/modem 连接。

同一门还固定默认 profile 的双端单实例合同：激活检查必须发生在制品发现和 WebView 初始化之前；Windows 的会话级主互斥体必须配合 PID marker，只允许激活真正持有默认 profile 的原生窗口，不能误选显式 `--data-root` 的隔离窗口；macOS 继续按 bundle identity 激活。第二次默认启动必须恢复并前置已有窗口后退出，隔离 profile 仍允许并行。

## 构建与平台门

Windows release：

```bat
build.bat
```

它先构建 React/Vite bundle，再构建整个 Rust workspace，并用 MinGW `objdump -p` 审计发布目录内所有 exe 和 dll；任何 `UCRTBASED`、`VCRUNTIME`、`MSVCP`、`CONCRT` 或 `MFC` import，以及已知 debug CRT 文件名都阻断。crosvm 的 r8brain 音频重采样器必须从固定 revision 的源码以同一 MinGW 工具链静态链接，发布闭包不得再携带 `r8Brain.dll` 或 debug MSVC CRT。根 `build_all.bat` 还验证发布目录包含十四个 HD 运行时 exe、`bin/ui/index.html` 和与当前 Cargo `webview2-com-sys` 产物逐字节一致的 `bin/WebView2Loader.dll`。MinGW `hd.exe` 实际动态导入该 loader，不能依赖开发机 PATH 中偶然存在的 Windows Performance Toolkit 副本；运行机仍需要 Evergreen WebView2 Runtime。

正式 Windows 分发还必须用 GNU `xtask package` 从显式 `--target-dir`、`--runtime-dir`、`--adb`、`--aapt2` 生成全新输出目录。门禁要求包内同时具有 crosvm/gfxstream、`vm`/`virtmgr`/`libbinder-rpc`、`adb.exe`/`aapt2.exe`、ADB sidecar、Android NOTICE 和静态链接 r8brain 对应的 `THIRD_PARTY_NOTICES/r8brain-free-src-LICENSE.txt`；输入不得是符号链接或非普通文件。打包器以清空后的环境、仅 Windows 系统目录 PATH 启动包内 HD、hdctl、Host、Worker、crosvm、vm、virtmgr 的 `--help`，以及 `adb version`、`aapt2 version`。通过后仍需保存 PE import audit，并用包内 ADB 对真实实例回读 `device` 与 `sys.boot_completed=1`；开发机 PATH 中的 platform-tools、build-tools 或 MinGW DLL 不计入发布闭包。

portable Rust 编译：

```bash
cargo check --workspace --all-targets --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --target aarch64-apple-darwin
```

交叉编译失败若来自缺少目标 linker/SDK，只能记录为 runner/toolchain 阻塞；不能把 Windows 编译结果填写为 Linux/macOS pass。最终平台门必须在原生 runner 执行 check、Clippy、build、package 和运行场景。

## 自动证据闭环

推荐入口：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- ai-cycle `
  --task automation/tasks/hd-p0-v2.json `
  --output out/ai/hd-p0-v2
```

无论门禁成功或失败，输出均保留：

- `logs/<gate>.log`：完整命令输出；
- `hd-gates.json`：版本化 gate report；成功的 Host smoke 会分别登记 `host-worker-smoke`、`capability-smoke`、`http-security-smoke`、`lease-smoke` 和 `diagnostic-smoke`；其中 `host-worker-smoke` 还会实际启动 `hd-device-sim --serve-v2`，校验 ready marker 的 launch hash/PID/启动标记、错误 bearer 拒绝、正确 bearer 命名管道 Ping 的完整身份以及受管终止；启动 `hd-rootcanal-adapter --serve-v2` 验证相同认证边界以及创建 GATT peer、关闭/开启广播、移除 peer 四个动作；启动 `hd-casimir-adapter --serve-v2` 验证 Type 2、Type 4、移除动作；启动五个 peripheral adapter 并验证各自 Guest 双向通道；并启动 `hd-adb-bridge --serve-v2` 验证 loopback TCP 经 crosvm `connect_vsock` 兼容控制入口到 SID 限定命名管道的原始字节往返和无孤儿退出；
- `artifacts/`：本轮 smoke 生成的 OpenAPI、迁移结果、宿主能力、租约审计、诊断 manifest 和 Worker 生命周期证据，不引用已经销毁的临时 data root；
- `readback.json`：机器事实底稿；
- `readback.md`：人类可读摘要；
- 每个相关仓库的 HEAD/dirty 状态。

涉及 crosvm/gfxstream/根发布链时：

```powershell
.\scripts\integration-quality.ps1 `
  -Task automation/tasks/workspace-integration.json `
  -Output out/ai/integration
```

脚本只编译 C++/Rust 测试目标，不运行测试 harness。`-RunRootBuild` 明确授权后才执行完整根构建；已有脏文件不会被自动清理、reset 或覆盖。

## Windows Android 15 真实 Guest 门

当前 Windows Player 的可见标题栏是独立顶部 WebView，不是 Win32 按钮层。下述门禁中的“原生标题栏/原生关闭按钮”历史措辞均按“最终 HD 根窗口中的产品标题栏”解释：窗口树必须证明 `HD_NATIVE_TITLEBAR_V1` 为 0、顶部 `WRY_WEBVIEW` 为 30 px、Android `NativeDisplayHost` 从其下方开始，且标题栏点击前后没有全局 busy 广播、无变化布局提交或冗余跨进程焦点切换。

指针动画压力使用常见 125 Hz（8 ms）Host 采样，验证与 60 Hz 显示周期不整除时仍保留分数节拍余量；source 与 present 平均间隔都必须不超过 20 ms。source 平均值必须使用独立的有效 enqueue interval 计数，且该计数必须与 present interval 数完全一致；只有严格递增的非零 timestamp 才是有效 source interval，缺失、重复或倒退都不得用 present 帧数作分母压低平均值。若输入合并退化到约 41 Hz，即使黑帧数和 gfxstream drop 都为 0 也不能通过发布门。

gfxstream 的落盘 frame metrics 必须记录实际 Vulkan `present_mode`，且真实 Guest 门只接受 `fifo`、`mailbox` 或 `immediate`。该字段与 Host present latency、source cadence、swapchain recreate/out-of-date 一并保留，避免用“提交成功”推测 DWM 呈现策略。

`cpu_readback_bytes` 与 `software_blit_count` 必须来自 gfxstream 实际事件计数，禁止在 metrics JSON 中硬编码 0。普通指针、窗口和标题栏路径要求两个计数的增量都为 0；允许非零拷贝的诊断录屏必须产生正的 readback byte 增量并单独记录。录屏停止后跨两个独立 metrics 发布周期触发普通 Android present，两个计数都必须保持不变，证明读回管线已退出且 Player 恢复零拷贝。

Windows MinGW 的 `hd_display_telemetry_windows_smoke` 不启动 Guest，直接注入 4096 bytes CPU readback、1 次软件转换、两个 source timestamp 和 FIFO present，随后读取原子 metrics 文件并精确回读这些非零值。它用于证明 telemetry 运行时没有把真实计数重新写成常量 0；真实 Guest 的“普通路径为 0、录屏范围为正、录屏后重新为 0 增量”仍必须由 `windows-real-guest.ps1` 单独验证。

黑屏门必须区分两个阶段：允许非零拷贝的诊断录屏检查 gfxstream source ColorBuffer，独立的 64×64 降采样屏幕探针检查最终 WebView2/Win32/DWM 合成区域。两项在不同的手势轮次运行，防止屏幕采样读回污染 cadence 结果。交互桌面可用且 Android render HWND 未被遮挡时，指针最终合成探针至少取得 20 帧、采集失败数为 0，整帧近黑数和最大连续近黑数都必须为 0；随后还要在另一轮 60 Hz 最终合成采样中物理点击 WebView 侧栏按钮 6 次，证明点击前后的焦点都精确指向当前 crosvm 输入 HWND、Android swapchain 不重建且无近黑帧。锁屏、非交互会话或 Android 区域被遮挡时，真实 Guest 发布门必须硬失败，不能把 pending 当成通过。

125 Hz 指针动画的性能门必须同时覆盖 source enqueue、post-worker 排队、post-worker 工作和成功 Vulkan present 四段。source 与 present 平均间隔都不得超过 20 ms，任何一段都不得出现 50/100 ms 停顿；post-worker 排队和工作必须取得非零样本、最大值不得超过 33.333 ms，超过 16.667 ms 的样本最多各 2 个且超过 33.333 ms 必须为 0。独立源录屏中的近黑帧、连续近黑帧和超过 100 ms 的源帧间隙均为发布硬失败，编码帧不得少于 12、drop 必须为 0，且 Host 编码帧数必须与 MP4 sample table 完全一致，避免用一两个静态帧或不完整文件掩盖用户可感知的卡帧。最终 DWM 采样还必须对连续拖动画面计算帧指纹：至少 4 个不同帧和 4 次切换，最长连续相同帧不得超过 8 个 16 ms 样本；静态标题栏与旋转探针只检查黑帧，不套用动态冻结门。

Windows Player 的 125 Hz `WM_MOUSEMOVE` 必须从 HD UI/DWM 线程异步投递到 crosvm，再由 crosvm 按显示刷新率合并；不得为每个移动同步等待 crosvm GUI/渲染线程。按下、抬起和滚轮继续使用 20 ms 有界同步交付。异步投递失败也必须计入 `HD_POINTER_FORWARD_FAILURES_V1`，不能像旧路径一样静默丢弃移动。

旋转门必须在最终 HD Player 已挂载 gfxstream 与 crosvm 子窗口后执行，而不能只在 UI 启动前读取 Android `mRotation`。必须覆盖横屏、反向竖屏、反向横屏并最终恢复竖屏；每个方向都要求 Android rotation、Player `NativeDisplayHost` 宽高关系、gfxstream render extent、crosvm input extent、版本化 applied viewport/rotation 与跨线程投影确认属性同步提交，并在独立 60 Hz DWM 探针中保持采集失败和近黑帧为 0。每个方向的 crosvm 日志最多允许一次 swapchain recreate，防止最终几何正确掩盖一次普通事务后又被 Worker force 的卡顿。每个方向还必须由 Windows `SendInput` 点击不同的 Host 左上象限点，再由同一 Guest 的 `getevent` 回读完整接触和物理坐标象限，依次证明右上、右下、左下、左上的 orientation-aware 投影，不能只用属性或静态公式推断鼠标正确。DWM 最小 20 帧门按实际样本数等待，最多 1 秒；不能用固定 250 ms 等待后同时要求 20 帧制造健康实现误报。恢复竖屏后还必须执行最大化→横屏旋转→原生 `WM_SIZE` 重放→还原→竖屏恢复：最大化期间允许等比黑边，但原生控制器不得回退到旧竖屏比例；普通窗口还原后 Android 必须充满标题栏下方内容区且无黑边，整段 DWM 采样仍要求近黑帧为 0。完成后才能继续 resize 与指针门，防止一次错误投影污染后续证据。

`scripts/windows-real-guest.ps1` 必须使用独立空 data root、Windows GNU 运行闭包和与 Host 架构匹配的 AOSP Android 15 产品闭包。Windows 的标题栏、侧栏和内容 WebView 必须共享 `data-root/cache/webview2`，禁止回退到可执行文件旁的 `hd.exe.WebView2`，使两次独立回归不会复用浏览器、GPU 或 shader cache 状态。`-RunActions` 在进入设备控制前必须递归枚举 Worker 下的 crosvm 进程，并通过 Win32 顶层窗口审计证明 `hidden=true` 的后端 surface 可见窗口数为 0；诊断 runner 不启动 Player，绝不能弹出只有 Android 渲染区域、没有 HD 标题栏和侧栏的独立窗口。产品可见画面只允许由 `hd.exe` 的 `NativeDisplayHost` 承载。Windows 与 macOS 都使用 gfxstream 原生宿主显示路线；Windows run manifest 必须为 `HD_FRAME_REQUIRED=0` 且不存在 `HD_FRAME_BROKER_V2`，零拷贝指标直接来自成功的 Vulkan present。侧栏只是 Android 表面的 WebView 覆盖层；开关侧栏不得重复提交未变化的原生显示 bounds、改变 Android viewport 或触发 swapchain recreate。导航键必须实际覆盖 `recent` 的多缓冲轮转，并在同一 Ready run 上再次验证 produced/imported/presented 全相等、drop/readback/software-blit 全为 0，同时要求 crosvm 日志中的 `HD display selection rejected unmapped color buffer` 为 0；该证据必须写入 `display_selection`，防止异步 scanout 资源轮转再次造成闪烁、黑屏或纵向压缩。`-RunScreenRecording` 允许录屏范围内的有界 readback，但必须在开始时立即捕获当前静态帧；Windows Vulkan readback 必须报告 `y-direction 1`，生成的 1080×1920 MP4 不得上下颠倒，停止后 Player 仍保持同一 run 的零拷贝 present。`-RunUiDisplayInput` 还必须从隔离副本启动最终 `hd.exe`，证明原生标题栏没有主/副屏选择器，确认主屏 crosvm 输入 HWND 与 gfxstream `subWin` 同时挂载在 `NativeDisplayHost`，并使用 Win32 `SendInput` 点击真实 gfxstream 渲染子窗口；Android `getevent` 必须同时回读坐标轴和按下/释放，最后通过原生关闭按钮退出而不改变 Ready run。该物理输入门必须在已解锁的交互桌面运行；若 `LockApp`/`LogonUI` 使命中目标成为 Explorer `LockScreenBackstopFrame`，runner 必须保存 hit-test 父链、用直达 gfxstream 渲染子窗口的诊断点击确认完整转发与 virtio-input/Guest 链，并以明确的环境门错误保持失败，不能把诊断点击冒充 `SendInput` 通过。Windows Player 还必须执行 UI 关闭/重开门：关闭前后实例保持 Ready、ADB Ready 和同一 run ID；关闭后 Worker 的隐藏 parking host 必须同时包含不可见的 crosvm 输入 HWND 与 gfxstream `subWin`，可见 crosvm/gfxstream 顶层窗口均为 0；重开后两者必须同时重新成为新 `NativeDisplayHost` 的子窗口，原生标题栏继续位于显示宿主上方。该门还验证四向旋转、导航/电源、定位、电池、网络、固定 r14 三轴姿态、Bluetooth/HCI、NFC、UWB framework policy 与 modem 动态状态；正式 modem adapter probe 必须包含 `runtime-modem-unsolicited-v2`，Host 必须把该 feature 作为启动要求。modem 不能只回读 Host record，必须由 unsolicited AT 使 Guest telephony 精确观察 `310260`、运营商名称、`IN_SERVICE` 和信号 17 对应的 GSM `-79 dBm`。UWB 没有活跃 FiRa 应用 session 时只记录 HAL/AIDL/framework、国家码和 typed state，不得宣称 Guest 已观察距离；显式 `-RunUwbFira` 必须通过 AOSP r14 framework shell 打开 session 77，观察 3.21 m、status 0，再停止并关闭会话。`-RunLocationRoute` 必须与 `-RunLocationProbe` 同时使用：除路线状态、暂停稳定和 Guest 交付序列外，还要由同一个真实 GPS framework 探针逐字段回读路线点，门后卸载且不使用 mock provider。`-RunBugreport` 可单独执行，但必须自行启动到 Ready，并通过 typed Host/Worker API 生成完整 AOSP dumpstate。bugreport 记录和文件必须绑定当前 instance/run，位于受管 diagnostics 目录，文件名包含 Host 生成 UUID，限制 22 字节至 256 MiB、SHA-256 精确一致、ZIP 至少含主 `bugreport*.txt`，Windows DACL 仅当前用户与 SYSTEM；采集后必须保持同一 run/ADB Ready，关停后按 data root 过滤的进程为 0。失败门也必须保存证据并完成隔离清理。

Player 页面切换、瞬时 1×1 布局和最小化是 HD 本地合成状态，不是 Guest 显示生命周期。它们只能隐藏 `NativeDisplayHost` 父窗口，必须保留版本化 DisplaySession、gfxstream render 与 crosvm input 子窗口及最后已呈现帧；恢复同一几何时只显示父窗口，不得执行 `SetWindowPos`、强制 viewport、detach/attach 或 swapchain rebuild。隐藏期间允许等 revision heartbeat 保活，但没有现有 session 时不得在隐藏页面后台新建 attachment。Resume、通用 Restart 和设置保存触发的 Restart 同样保留 Host session，并以立即 heartbeat 刷新 frame generation；只有实例/scanout 切换、Stop/Delete/Powerwash、原生应用生命周期 suspend 或 UI 关闭才允许释放。

125 Hz 指针转发和 `WM_SIZE` 实时缩放不得在每个事件中枚举整个子窗口树或分配 UTF-16 类名字符串。`NativeDisplayHost` 必须缓存 crosvm input HWND，根窗口缩放控制器必须缓存 input/render 两个 HWND；每次命中仍验证 `IsWindow`、直接父窗口和绑定父 HWND 的子窗口 cookie，句柄销毁、复用或 scanout 重挂后回退到无分配类名扫描并更新缓存。Host→crosvm 的 `WM_MOUSEMOVE` 必须从 HD UI/DWM 线程异步投递，交由 Windows/crosvm 消息与显示节拍合并；按下、抬起和滚轮使用 20 ms 有界同步交接。真实门必须同时证明 125 Hz 输入下 source/present 平均间隔不超过 20 ms、没有动画后回放的陈旧坐标、转发失败为 0，并在首次点击、重复点击和 UI 重开后分别验证焦点；目标 crosvm HWND 已持有焦点时不得重复 `AttachThreadInput` 或 `SetFocus`。压力门本身必须使用 Windows `SendInput` 按下与真实光标移动，经正常捕获链路进入 crosvm，不能用诊断 `PostClientDrag` 直接灌入渲染 HWND。实时拖动期间 input/render 子窗口和 Vulkan swapchain 必须完全静止，只移动 HD 自有父 viewport 来居中或裁剪保留帧，不得每个 `WM_SIZE` 向 gfxstream 队列投递异步 `SetWindowPos`。鼠标释放只允许一次 force commit；120 ms settle 阶段必须用普通最新值事务，让 crosvm 对已应用尺寸去重而不是再次强制重建。真实门必须精确回读首次运行、交互缩放及 UI 重开后的缓存 HWND，并拒绝一次拖动产生多个 swapchain recreate。

```powershell
.\scripts\windows-real-guest.ps1 `
  -RunActions -RunUwbFira -RunBugreport -RunLocationProbe -RunLocationRoute `
  -LocationProbeApk <absolute-path-to-hd-location-probe.apk> `
  -Output out\windows-android-real-guest
```

## macOS 签名 Android 分发门

每个携带 Android 的 macOS 候选必须先通过三层互不替代的证据：

1. `macos-android-package-contract-smoke.sh` 在创建输出前拒绝 development 零/双 Android 源、release direct image、release 缺少 signed store 与 store-root 符号链接；
2. `macos-android-artifact-store-smoke.sh` 对完整 store 做正向验签，并拒绝错误 channel、额外文件、rootfs 篡改、错误 trust、release data-profile 不匹配、仓库根和嵌套父目录符号链接；
3. `macos-android-distribution-smoke.sh` 从独立解包的最终归档连续启动两次 Android 15，只用包内 Host/Worker/crosvm/gfxstream/ANGLE/ADB/aapt2，要求两次 Ready、网络、userdata 持久化、frame generation 单调、非录制状态零 CPU readback/software blit 和停止后无进程泄漏；正常停止必须保持 Android 专用的固定 `adb shell reboot -p`，由 Guest console 回读 `reboot: Power down`、状态收敛为 Stopped，且不得调用 Microdroid Full-debug 专用的 `adb root`。多显示子门必须建立主屏和至少一个副屏，回读稳定 UUID→scanout→Android display、分辨率、刷新率、物理/逻辑 DPI 与逐屏 virtio multitouch，并证明切换只改变单原生 surface 的呈现和输入目标。完整 bugreport 子门必须通过 typed Host/Worker API 调用固定 `adb bugreport`，要求实例/run 绑定、0600、22 字节至 256 MiB、ZIP 主文本成员、SHA-256 精确匹配和产物后 Android/ADB 仍 Ready，且长操作只为该命令保留 10 分 30 秒 IPC 边界。录屏子门必须通过正式 Host/Worker socket 为明确显示 UUID 启动 gfxstream recording callback 和硬件 H.264 writer，生成同时含 `ftyp`、`mdat`、`moov` 且样本表至少两帧的 MP4，要求创建中和完成后的用户视频均为 0600，媒体尺寸匹配所选显示，媒体时长不短于墙钟时长的 75% 且不超过墙钟加一秒，复验 Worker SHA-256，把视频保存在证据目录并清理用户 `Movies/HD` 中的测试文件，再停止录制并继续完成四向旋转，证明没有恢复 Guest virtual-display `screenrecord`。它还必须产出 `macos-android-aosp-controls` 与 `macos-android-device-controls`，前者用 Android framework/kernel 独立回读四向旋转、导航/音量/电源、电池与网络整形，后者证明真实 fixed-location HAL 使用固定 Guest 串口且定位高度与精度没有被 ADB mock provider 降级；固定 r14 传感器只通过 Guest 内置 AOSP motion 命令验收三轴姿态帧和 SensorService `NORMAL`，不得把未发布的独立/定时传感器注入写成通过。字段级交付由 Unix Guest 订阅黑盒契约补充约束。启用 Bluetooth 时必须产出独立 `macos-bluetooth-real-guest`，证明包内正式 AOSP RootCanal、Guest 非零 H4、虚拟 GATT peer 创建/广告开关/移除动作，以及动作后和第二次启动后的 Bluetooth HAL、framework binder 与 `ON` 状态均存活；启用 UWB 时必须产出独立 `macos-uwb-real-guest`，证明包内正式 FiRa v2 UCI component、Guest 非零 UCI，以及首次和第二次启动后的 UWB HAL、AIDL/framework service、`READY` 设备状态与国家码；启用 NFC 时还必须产出独立 `macos-nfc-real-guest`，证明包内正式 AOSP Casimir、Guest 非零 NCI、Type 2/Type 4/移除动作，以及动作后和第二次启动后的 NFC HAL、Cuttlefish HAL 进程与 framework binder 均存活；启用 Modem 时必须产出独立 `macos-modem-real-guest`，证明 Guest-CID host-vsock UDS、vendor RIL、Radio HAL、telephony framework 和测试运营商 `00101` 在两次启动均成立，且停止后四个 adapter 均无泄漏。

固定定位字段门必须临时安装同一个架构无关、test-only 的 LocationManager 探针；探针遵守 Android 15 前台定位规则，只订阅真实 `GPS_PROVIDER`，不得创建或启用 mock provider。Windows `-RunLocationProbe -LocationProbeApk <apk>` 与 macOS `--location-probe-apk <apk>` 都必须精确回读纬度、经度、高度、精度及两个字段存在标志，要求 Guest 交付序列至少包含初始与受控样本，并在继续测试前卸载探针。路线门重新安装同一探针，精确回读 KML/GPX 路线点后再次卸载；暂停/继续状态和交付计数只作为附加证据。仅有 typed action 成功、`dumpsys` 字节数、`CMD_GET_LOCATION`、Host 交付计数或 Worker 路线状态不能代替 framework 回调。

macOS UWB 子门还必须通过 AOSP r14 framework shell 打开 session 77，观察 3.21 m 与 status 0，再停止并关闭；HAL/AIDL/framework 存活、国家码、非零 UCI 或 Host typed state 均不能代替该真实 Guest 测距回读。

副屏集合是重启敏感的每实例设置：运行中保存必须走 UI 的“保存并重启”事务，Host API 必须拒绝绕过停止阶段的直接更新。当前 macOS Android 15 上，crosvm 运行时 `add-displays` 的成功响应不能证明 Android HWC 已建立逻辑显示；出现 `wm size 0x0` 或 `wm density -1` 时不得持久化配置或宣称热插拔可用。冷启动多显示器门仍是发布必需项。

启用 HOGP 键盘模拟时还必须产出独立 `macos-bluetooth-hogp-real-guest`，从 Android 设置发现和配对开始，完整证明 LE 加密、ATT MTU 分页、Report Map/Reference、input CCCD 订阅以及 Guest `/dev/input` 中 `KEY_A DOWN`/`KEY_A UP` 输入事件。

Microdroid `match_host` 必须使用独立 data root 和最终安装包执行：先证明存在任一 CPU 租约时独占启动被 Host 原子拒绝，再在空租约下启动并回读 Guest online/present CPU 数等于 Host 逻辑 CPU 数；运行中启动 one_cpu Microdroid 和 Android 都必须被拒绝，停止后 CPU 租约必须归零且普通实例重新可启动。性能证据必须以同一 Payload、内存、调试级别分别运行 one_cpu 与 match_host，记录启动时间、Payload 墙钟/CPU 时间、Host CPU、温度和功耗；不能用 `--cpu-topology match_host` 字符串或能力卡片替代真实 Guest 与租约证据。

Windows x86_64 Microdroid 必须运行 `scripts/windows-microdroid-real-guest.ps1`。该门只接受独立空 data root、Windows GNU `hdctl/Host/Worker`、正式 `vm/virtmgr/crosvm/libbinder-rpc` 运行闭包和 `vsoc_x86_64` Guest 制品；EmptyPayload 模式必须回读 `run-microdroid`，指定 `-PayloadApk` 时必须先通过 Host 的 v3 Payload 预检和上传 SHA 复验，再回读 `run-app`、受管 upload 路径、唯一 `payload.idsig` 与 `assets/vm_config.json`。runner 必须把 debug level、CPU topology、内存和 0/10–4096 MiB 加密存储作为显式候选输入，并从每轮 manifest 精确回读对应参数。Full-debug 每轮要求唯一 run、有效 CID、Payload 验签、Guest boot completed、Host Payload Ready、SDK 35、Guest power down 和完整 Host→Worker→vm→virtmgr→crosvm 进程拓扑；加密存储还要证明只在首轮格式化、重启保持 Host 文件身份和 ext4 UUID。None-debug 必须在 10 秒内 Ready，console/Guest log 均为空，不发布 ADB serial/ready，并明确使用 force stop。所有模式在 delete 和 `shutdown --stop-all` 后按 data root 过滤的相关进程必须为 0；失败时 runner 必须在删除实例前保存完整 run 证据。源码契约或一次手工启动记录不能替代该门。

```powershell
.\scripts\windows-microdroid-real-guest.ps1 -Output out\windows-microdroid-empty -Cycles 3
.\scripts\windows-microdroid-real-guest.ps1 -Output out\windows-microdroid-uploaded -PayloadApk C:\absolute\payload-x86_64-v3.apk -Cycles 3
```

Microdroid extra APK 必须使用最终安装包和专用签名 Payload 执行。Host 范围先运行 `hd-microdroid-payload-inspection-smoke`，证明存储/Deflate 配置的声明数解析，以及损坏 JSON、超过 8 项、空路径的有界拒绝；UI/能力 smoke 必须固定实例级 `payload_extra_apk_count`、未选满阻断和 `microdroid_extra_apk_count_mismatch`。最终 Guest 门中，主 `vm_config.json` 声明至少两个有可区分内容的 extra APK，Host 规格只保存受管 UUID/SHA 和声明数量，run manifest 中只能出现受管上传和 run idsig 路径。门禁依次证明数量少/多、重复 upload、摘要篡改、无 v3/v3.1 signing block 与主 Payload 身份变化均失败；成功路径从 Guest `/mnt/extra-apk/0`、`/mnt/extra-apk/1` 回读对应内容和 fs-verity 身份，第二次启动保持顺序，停止后 idsig 子进程、FD 和临时 run 文件按保留策略收口。不得把 Host inspection、`--extra-apk-override` 字符串或文件存在当作 Guest 挂载证据。

同一 Payload 预检还必须解析 AOSP `apexes`、`prefer_staged` 与 `hugepages` 字段。在版本化 APEX 产品闭包、兼容性及真实 Guest mount 证据完成前，非空 APEX 列表、Host-staged APEX 解析和 macOS 无效 hugepage 请求都必须在上传及 Worker 启动复验阶段拒绝，不能延迟到 virtmgr/Guest 或受环境变量影响。

专用 QA 材料必须由仓库脚本从已审计模板生成到全新目录；脚本固定使用调用方提供的 JDK 和 Android build-tools，为主 Payload 写入恰好两个声明，为两个不同 APK 写入可区分资源，生成共同临时 QA 证书的 v3 签名版本及一个未签名负例。输出必须通过 `apksigner verify --verbose --print-certs`、`zipalign -c -p 4`、声明/资源 SHA-256、无私钥/符号链接以及文件权限 `0600` 复核；临时 PKCS12 私钥不得进入输出。示例：

```bash
sh scripts/microdroid-extra-apk-materials.sh \
  --main-template /absolute/path/to/template-payload.apk \
  --extra-template-0 /absolute/path/to/first-template.apk \
  --extra-template-1 /absolute/path/to/second-template.apk \
  --android-build-tools /absolute/path/to/android-sdk/build-tools/36.0.0 \
  --java-home /absolute/path/to/temurin-21/Contents/Home \
  --output /absolute/path/to/fresh-material-directory
```

专用最终包门禁调用如下；两个 `--asset-path-*` 由 runner 同时从 Host APK 与 Guest zipfuse mount 读取并比较 SHA-256，不能只传预期字符串。无 v3/v3.1 signing block 的第三个 APK 只用于负向启动前拒绝。该 runner 会先完成数量不足/超量、重复 upload、摘要错误与签名错误，再执行两次真实 Guest；它只终止自己随机 data root 中的进程。主 Payload 身份变化继续由 `macos-microdroid-payload-changed-smoke.sh` 独立覆盖。

```bash
sh scripts/macos-microdroid-extra-apk-smoke.sh \
  --app /absolute/path/to/HD.app \
  --output /absolute/path/to/fresh-extra-apk-evidence \
  --payload /absolute/path/to/main-payload-with-two-extras.apk \
  --extra-apk-0 /absolute/path/to/extra-0-v3.apk \
  --extra-apk-1 /absolute/path/to/extra-1-v3.apk \
  --asset-path-0 assets/hd-extra-0.txt \
  --asset-path-1 assets/hd-extra-1.txt \
  --invalid-signature-extra-apk /absolute/path/to/extra-without-v3.apk \
  --development-package
```

启用 HCI 抓包时还必须产出独立 `macos-bluetooth-hci-capture-real-guest`：请求只能是 1,000–30,000 ms 的 typed 有界动作，路径和 capture UUID 由 Host/RootCanal 生成；真实 Android Guest 必须在抓包窗口内产生 H4 流量，`packets_captured` 必须大于零。门禁要求标准 btsnoop HCI UART/datalink 1002、0600、最大 4 MiB、sidecar/UUID/实际大小绑定，并实际生成诊断 `.tar.zst`，复验 btsnoop 原字节、sidecar JSON 语义、逐文件 manifest SHA、manifest SHA 与 archive SHA。诊断解包使用调用方显式提供的非符号链接 arm64 `--zstd`，版本和 SHA-256 写入结果，不能依赖交互式 PATH。第二个 run 必须把 `last_bluetooth_hci_capture` 清为 `null`，避免 UI 错称旧文件仍属于当前诊断包。

`hd-diagnostics-product-smoke` 必须同时放入含非 UTF-8 packet 的合法 btsnoop 和含 secret 的文本日志：只有严格匹配 `run/<UUID>/components/rootcanal-hci-<UUID>.btsnoop`、标准头、未截断且不超过 4 MiB 的显式抓包可保持原字节，其他 JSON/文本继续执行脱敏；两种性质必须在同一个 tar.zst 黑盒中同时成立。

Unix `xtask smoke` 对 DeviceSim fixed-location、RootCanal/UWB/Casimir/Modem 的鉴权、Ping、定位四字段/peer/UCI/标签/AT 状态机、socket/FIFO 权限和生命周期验证属于正式组件契约门；fixed-location 黑盒必须让 Guest 发出 `CMD_GET_LOCATION` 并精确回读经纬度、高度和精度。该门还必须证明活动 Unix socket 不能被第二个 adapter 接管，正常 SIGTERM 清理自身 device/inode，而异常退出留下的陈旧 socket 可被安全恢复。组件门不替代真实 Android HAL/framework 数据面门。

Microdroid 必须对同一 archive 另跑 `macos-microdroid-distribution-smoke.sh`，覆盖 Empty 与 uploaded Payload 到 Ready、`debug=full`/`debug=none`、正常关机、`shutdown --stop-all` 和无进程泄漏。`Full` 下两种 Payload 都必须在 Payload Ready 后通过各自回环端口进入 ADB Ready，并以 10 秒有界的包内 `adb shell getprop ro.build.version.sdk` 回读 SDK 35；仅有 `get-state=device` 不构成数据面通过。每个 Full Empty/Uploaded（包括重启与并发场景）停止时都必须由 Guest console 回读 `reboot: Power down`，实例状态为 Stopped、活动 run 为 null，Worker 日志不得出现 `worker.stop.adb_power_off.failed`、`worker.stop.power_button.failed` 或 `process.terminate.started`。`None` 必须从 run manifest 回读 `--debug none`，保持 ADB serial 为 null、`adb_ready=false` 且不得启动 adbd debug policy，并在 10 秒启动预算内完成；无 ADB 的清理边界仍由显式 `shutdown --stop-all` 回收。EmptyPayload 的 64 MiB 加密存储必须证明首次格式化、重启不重复格式化且复用相同 ext4 UUID/Host 文件身份；另一实例必须得到不同 ext4 UUID 与密文摘要。开发签名与 development-unencrypted 只构成 QA 通过；Developer ID/notary、生产 Ed25519 根和 signer、metadata-encrypted Guest 输入及持久硬件 KeyMint 仍必须分别取证，不能由开发门禁推导为正式发布通过。

有限 Payload 还必须增加最终包真实 Guest 门：一个 Payload 在 Ready 后返回 0，另一个返回固定非零值。成功路径要求宿主 `vm` 的两个日志和进程退出码形成严格三元证据，实例/run 自动收敛到 Stopped、run `exit_code=0`、活动 run 清空且所有租约/进程/端点释放；失败路径要求 `Failed / microdroid_payload_failed` 并保留原始 Payload 退出码。Guest console 中注入同名完成文本不能改变结果，用户 stop 与自然退出竞态必须只有一个最终 result 和一次资源清理。

Microdroid console challenge 的 Host 黑盒必须在 Unix/macOS 运行 `cargo run -p hd-runtime --bin hd-microdroid-console-challenge-smoke`，证明 owner-only FIFO 不会 EOF、固定帧由 32 字节随机 nonce 构成、合成消费者的精确响应被验证，以及未确认、nil id、第二次发送、超时、非 FIFO 替换均拒绝；成功和失败后 FIFO 必须消失，0600 audit 只保留 nonce SHA-256。该黑盒不能替代真实 Guest：最终包还必须用 Full-debug 专用受信 Payload 从 console input 读取一次 challenge 并向当前 console 输出精确 response，Host action 成功后复验 audit、run/instance 身份，停止后 `lsof +L1`、FIFO 和进程均无残留。None-debug、Android 和通用 Web UI 必须继续没有该入口，且任何 API 都不得接收调用方 console 文本。

```bash
sh scripts/macos-microdroid-console-challenge-smoke.sh \
  --app /absolute/path/to/HD.app \
  --output /absolute/path/to/fresh-console-challenge-evidence \
  --payload /absolute/path/to/trusted-console-challenge-payload.apk \
  --development-package
```

最终安装包还必须分别运行五个真实负向门禁：`macos-microdroid-death-reason-smoke.sh` 保持 APK/签名块结构并破坏 v3 digest，要求 Guest 返回 `MicrodroidPayloadVerificationFailed`；`macos-microdroid-payload-changed-smoke.sh` 先用原始有效 v3 Payload 到 Ready，再在同一实例换成语义等价、独立有效 v3 签名的 Payload，要求 Guest 返回 `MicrodroidPayloadHasChanged`；`macos-microdroid-invalid-config-smoke.sh` 使用与 AOSP 一致的 no-task 配置和独立有效 v3 签名，要求 Guest 返回 `MicrodroidInvalidPayloadConfig`；`macos-microdroid-service-connection-smoke.sh` 只移除隔离 virtmgr 为本次 CID 创建的 owner-only Binder-RPC socket，要求 Guest 返回 `MicrodroidFailedToConnectToVirtualizationService`；`macos-microdroid-fault-injection-smoke.sh` 杀死实际 crosvm VMM，要求原 run 为 `failed / microdroid_runtime_failed`，随后同一 Worker、新 Worker与 Host 重连均恢复。五者都必须保留 typed instance/run 结果、原始 vmclient/virtmgr 日志、0600 门禁报告，并证明隔离 data root 无进程泄漏。`VirtualizationServiceDied` 表示连接建立后的 Host 服务死亡，必须归入运行时基础设施故障，不能复用“Guest 启动连接失败”的修复提示。

Microdroid 的 Payload Ready 不得等待 ADB 超时；Worker 必须先发布 `Ready`，再以绑定当前 run 的 30 秒后台任务探测 adbd。只有精确 serial 报告 `device` 才能发布 `adb_ready=true`，超时、停止或新 run 都必须保持 false 且不得拖慢启动。
分发门中的 Empty 与上传 Payload 并行启动必须在 10 秒内全部到 Ready，随后两者都必须完成独立 ADB shell；旧的同步 30 秒探测、共享 serial、仅握手成功或 Darwin 非阻塞子流截断都必须被该门直接拒绝。

安装升级另有三项必需门禁：`host-runtime-inactive-takeover` 使用真实不同摘要的旧 Host 验证五秒内空闲接管；`host-runtime-active-deferral` 在 Ready Microdroid 运行期间验证旧 Host、Worker 和 run 身份不变，stop-all 后才切换到新摘要；`host-runtime-upgrade-contract` 拒绝相同 old/new Host 和符号链接 old-app。三者必须使用隔离 data root 且结束后无进程泄漏，不能用“客户端 health 成功”代替身份回读。

macOS 多 WebView 的实例交互必须另跑并合入 `web-ui-concurrency`：

```bash
./scripts/macos-ui-concurrency-smoke.sh \
  --output /absolute/path/to/ui-concurrency-evidence
```

该门禁真实运行 UI 契约进程，验证快照唯一代次、选择或状态变更后的陈旧响应拒绝、重复完成事件隔离、侧栏操作目标实例绑定，以及设置/Payload 异步结果不覆盖已切换的实例。它是状态机与产品命令边界验证，不替代 macOS 辅助功能权限下的物理鼠标、键盘和窗口截图验收。

`hd-trackpad-product-smoke` 是不启动 Guest 的正式数据面黑盒：它创建平台 owner-only 临时端点，用独立客户端模拟 crosvm 连接，发送归一化 DOWN 事件，并逐字段复验 little-endian `EV_ABS/EV_KEY/SYN_REPORT` 顺序。`hd-trackpad-queue-smoke` 还必须证明 UI→Host 队列固定上限、Move 只保留最新坐标、接受 Down 时已经为 Up 预留空间，并且实例选择变化不会把释放事件改投其他实例。`xtask smoke` 必须同时收集 `trackpad-smoke.json` 与 `trackpad-queue-smoke.json`；真实 Guest 发布门还要证明启用该实例设备后 `/dev/input` 出现独立 indirect pointer，侧栏滑动与轻点生效，关闭侧栏不改变 Android 渲染区域。

Windows gfxstream 构建还必须生成并运行 `hd_host_recorder_windows_smoke.exe`。该进程动态链接本轮正式 `libgfxstream_backend.dll`，使用生产 Media Foundation writer 编码四帧确定性 BGRA，要求实际 H.264 transform 带硬件设备 URL，复验 MP4 顶层 `ftyp`、`mdat`、`moov` 和非空长度，并删除临时文件。编译成功、系统存在硬件编码器、只设置 `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` 或仅看到 `.mp4` 扩展名均不能替代该门；真实 Guest 发布门仍要验证所选 scanout、开始/停止/超时、窗口旋转、文件权限、播放时长以及停止后 Player 继续零拷贝。

安装应用的 Microdroid 长稳基线使用同一隔离 data root 连续运行真实生命周期：

```bash
./scripts/macos-microdroid-soak-smoke.sh \
  --app /absolute/path/to/HD.app \
  --output /absolute/path/to/fresh-soak-evidence \
  --cycles 20 \
  --development-package
```

每轮必须 create、到达真实 Payload Ready、force stop、delete，并确认 Worker/crosvm 已回收；最终 Host 也必须退出。门禁记录逐轮 Ready 耗时、Host RSS、文件描述符、线程和 data root 大小，Ready 预算为 90 秒；热身后分别限制为 128 MiB、16、8 和 16 MiB。development 参数只能用于明确标记的 development 候选，release 包必须在认证路径运行且拒绝该参数。20 轮是每个候选的快速长稳门，发布前仍需执行平台矩阵规定的 100 次生命周期与 2 小时长稳。

运行历史保留必须由 `xtask smoke` 中的 `runtime-storage-smoke` 固化，不接受只检查常量。门禁先构造超过 64 MiB 的已完成历史日志，要求原文件截断且 `.previous` 精确保留最后 16 MiB，再制造超过 20 轮的记录并验证最旧完成轮次被删、总逻辑大小不超过 2 GiB。启动前 retention 必须删除旧的已完成 run 中精确命名的 `initrd-android-hd.img` 以及 `microdroid-extra-0.idsig` 至 `microdroid-extra-7.idsig`，成功停止后的当前 run 也必须立即删除；`microdroid-extra-8.idsig` 等越界名称必须保留，没有 `result.json` 的活动 run 即使存在白名单文件也必须拒绝清理并原样保留。

低磁盘产品门不得用填满用户磁盘的方式制造条件。runner 创建并挂载独立 2 GiB APFS sparse bundle：

```bash
./scripts/macos-host-resource-admission-smoke.sh \
  --app /absolute/path/to/HD.app \
  --output /absolute/path/to/fresh-resource-evidence \
  --development-package
```

新 Android 实例仍声明 10 GiB。`hdctl resource-admission <UUID>` 必须在 2 秒内只返回单个 `host.resources=blocked` probe，不得返回 `probes` 集合或触发制品/设备发现；完整 `capabilities` 必须在 10 秒内返回资源阻断，start 在 5 秒内返回 `capability_blocked`，且不创建磁盘、不启动 Worker/crosvm。资源已阻断时，Host 记录 `capability.artifacts.deferred` 并把 `artifact.bundles` 标记为由 `host.resources` 延后，避免失败路径先哈希 16.5 GB aggregate；这不是跳过供应链验证，资源满足后的严格 start 仍必须完整验签和逐文件哈希。runner 只读取包内索引；signed store 的全量验签由独立 artifact-store/package/installed-guest 门禁承担，不能在这里重复后把哈希耗时误算为资源拒绝延迟。

同一门禁还必须确认 start 的 typed error 明确包含 `host.resources`、`required_disk_bytes=10737418240` 与 `disk_requirement_mode=new_instance_storage`；只返回 `capability_blocked` code 而没有可行动原因视为失败。消息必须说明其余制品、设备和认证检查被延后，并且不能把派生的 `artifact.bundles` 阻断再次列为独立根因。`hd-ui-contract-smoke` 同时固定资源专用 HTTP/client 路径、资源 probe 到性能设置页、代次淘汰、手动资源刷新、按需设备发现以及 start 继续调用严格 discovery 的契约，防止 UI 重构重新引入启动前的重复大镜像哈希。

`ui-snapshot-performance-smoke` 必须验证 `GET /v2/ui-snapshot` 在 2 秒控制面预算内以一次请求返回同一 store 枚举中的摘要与选中详情，并在选中 ID 已删除时确定回退到首个实例。`hd-ui-contract-smoke` 还要执行轮询策略矩阵：生命周期过渡 250 ms、稳定前台 Player 1 s、前台辅助页/失焦 2 s、最小化或原生 `Occluded` 5 s；源码必须消费 `WindowEvent::Occluded`、记录状态变更且不得重新出现 100 ms ticker 或 `list_instances` 后追加 `get_instance` 的 UI 组合。按不可见稳定态计算，请求从旧实现每分钟 480 次降为 12 次（97.5%）；该状态只降低控制面轮询，不得释放 DisplaySession 或改变 gfxstream 显示链，且实例切换的旧代次响应仍必须拒绝。

`ui-background-work-performance-smoke` 还必须固定 macOS 网络状态检查的页面生命周期：应用启动、Player、设置、诊断、失焦、原生遮挡和最小化状态不得启动外部 status 进程；进入并聚焦设备页时仅在缓存达到 30 秒后刷新，设备页前台最多每 30 秒一次。手动刷新和安装完成后的强制复验继续可用，但不得重叠；离开设备页会使旧代次响应失效，旧响应不得在后台补发新探测。按稳定 Player 计算，网络 status 外部进程从旧实现每小时 120 次降为 0。

macOS Player 性能门禁必须使用最终应用、真实 Ready Android Guest、可见原生 surface、60 Hz 和至少主副两个显示运行。稳定 60 秒后，UI 进程的多次采样中位数不得超过单核 10%，`sample` 的主线程绝大多数样本应阻塞在 `mach_msg`/事件等待；持续出现 `CFRunLoopAddSource` 热循环直接判定失败。`hd-ui-contract-smoke` 还要静态固定“单个 `Arc<EventLoopProxy<UserEvent>>` 共享”契约，禁止 WebView IPC、轮询和异步任务重新克隆原始 macOS winit proxy。该门禁只约束桌面壳控制面，不用降低 Guest 帧率、隐藏窗口或释放 DisplaySession 来制造低 CPU 结果。

`scripts/macos-ui-native-lifecycle-smoke.sh --app <HD.app> --output <fresh-dir>` 必须通过 Finder `open -n` 启动最终应用并使用隔离 data root。根 NSWindow、NativeDisplayHost 和 WebView 只能在 winit 首次 `Resumed`、即 event handler 已安装后创建；重复恢复不得创建第二套界面。根窗口还必须在 15 秒内聚合首屏必需 surface、调用原生显示并确认 `is_visible`，日志必须恰好出现一次 `ui.lifecycle.first_paint.ready`，不得出现 `visibility_rejected` 或 `timed_out`；超时判定的 14,999/15,000 ms 边界、已显示和正在关闭状态由 `hd-ui-contract-smoke` 固化。随后以同一 bundle identity 执行不带隔离参数的普通第二次 Finder 启动，必须只激活已有窗口，UI/Host 总数仍各为一且不得创建第二 data root；显式 `--data-root` 则属于允许并行的隔离 profile。观察期内 UI/Host 必须存活，日志不得出现 `tried to run event handler, but no handler was set` 或 `ui.lifecycle.startup.failed`，Player 也不得触发网络状态探测；最后必须走认证 shutdown 并证明零进程残留。

Android 安装包门中的 `adb_ready` 必须代表 Guest 已稳定可交互，而不是命中启动尾段的单次采样。`AdbClient::wait_ready` 必须连续观察 `sys.boot_completed=1`、`service.bootanim.exit=1`、`init.svc.bootanim=stopped`、bootanimation 进程不存在、主用户已解锁、Package Manager 可查询，以及 SurfaceFlinger、InputManager、WindowManager 已注册；稳定样本后必须通过镜像支持的有界 `cmd package wait-for-background-handler --timeout 5000`，同时禁止把全局 `wait-for-broadcast-idle` 当成硬门禁。退出请求不能单独满足 Ready。Worker 在设备/网络策略之后、显示配置之后都必须重新满足该合取；keep-awake 与显示配置按幂等、有界事务收敛，瞬时服务撤销允许重试，但失败时绝不发布 `adb_ready`。Windows 真实来宾门取得 `adb_ready=true` 后独立重跑后台 handler 探针，并为指针动画保存清空后的 `threadtime` logcat、gfxinfo、源帧录屏和 Host/gfxstream cadence；20 轮启停 soak 的任一回归都保留完整 run 日志。

同一 soak 还必须验证 Unix detached 子进程回收：每轮 Stop 后保留同一健康 Worker，Delete 后 Worker PID 必须彻底消失且不能处于 zombie，Host shutdown 后 Host PID 同样必须被 UI reaper 回收。仅记录 `worker.stopped` 事件但仍能通过 `kill -0` 观察到僵尸进程，视为产品生命周期失败。

同一 soak 以第 1 轮为热身基线，20 轮结束时整个隔离 data root 增长不得超过 512 MiB。该预算覆盖 userdata 的正常启动写入和最多 20 轮诊断记录，但不允许每轮重复保留确定性派生的 patched initrd；结果必须同时报告 data growth、Host RSS/FD/线程增长、零拷贝和 SwiftShader 禁用状态。短探针只能用于调试斜率；发布 gate 的命令与摘要必须反映实际执行轮数，`result.json` 的 `cycles` 字段必须与逐轮记录数一致。

macOS 严格工件校验性能另跑：

```bash
./scripts/macos-artifact-hash-performance-smoke.sh \
  --app /absolute/path/to/HD.app \
  --output /absolute/path/to/fresh-hash-evidence \
  --budget-seconds 20
```

该门禁直接读取 signed-artifact-store-v2 manifest 指定的完整 rootfs，用运行时同一 `sha256_file` 路径校验摘要。macOS 实现必须通过 no-follow 文件句柄调用 CommonCrypto 流式 SHA-256，不得缓存、抽样、跳过稀疏区或启动外部摘要进程；Windows/Linux 保持 portable `sha2` 路径。20 秒预算用于阻止每次 Start 在 VM 启动前额外等待约一分钟。

## 真实 Android 与零拷贝矩阵

每个平台专用 runner 必须自动执行：

1. 从空 data root 加载固定签名 Guest/Host bundle并回读所有 capability；
2. 冷启动到唯一 Ready 条件，保存 manifest/events/result 和诊断包；
3. 两实例并行，验证 CID/ADB port/disk/GPU/frame/设备 endpoint 隔离；
4. Home、Recent、Back、Power、Volume 的 Guest UI/系统状态回读；
5. APK 安装后用 package manager 回读精确包路径；
6. 四种方向、分辨率、DPI、30/60/90/120 Hz 与 Android rotation 一致；旋转先发送一次幂等锁，若 4 秒后仍稳定停留旧角度最多重申一次，同一 10 秒事务预算不延长；WindowManager 必须观察到目标 rotation 与 transition 完成，失败事务回滚；
7. VSync on/off、同适配器外部纹理、显式同步、三缓冲 generation；非录制状态的 CPU readback/software blit/编码计数均为零，显式录屏期间只允许所选 scanout 的有界限速读回，停止后计数必须停止增长；
8. 定位、电池、网络、当前 profile 发布的传感器能力、RootCanal、Casimir 及启用 adapter 的 profile conformance；
   固定桌面 Android 15 r14 必须通过 Guest 内置 AOSP motion injector 精确回读同一次姿态产生的 accelerometer、magnetometer、gyroscope 三组值和 SensorService `NORMAL` 模式，不发布 Light、proximity、独立值或定时覆盖。其他 profile 只有在真实 HAL 支持时才可发布对应 capability；届时持续时间必须单独证明 0 时长保持覆盖、200 ms 及以上到期恢复 HAL 默认值、不同传感器截止时间相互独立，以及旧截止时间不会清除同一传感器后续的新覆盖。不得用 UI 倒计时或请求成功代替 Guest 回读。
9. `hd-powerwash-product-smoke` 必须在隔离 data root 证明：Microdroid、活动实例、错误实例名、陈旧 revision、第二个未处理备份和非普通路径均拒绝；powerwash 使用同卷原子 move 且保留完整摘要，恢复把当前数据变成唯一 rollback backup 并可再次交换；模拟崩溃发生在 powerwash rename 后、restore 两次 rename 之间和 discard 删除后时，重启恢复分别安全提交、回滚或完成记录，不得同时丢失 source/current。真实 Guest 门还必须写入 userdata 标记，powerwash 后证明标记消失且实例设置不变，再恢复并证明标记回来。
9. crosvm/Worker/Host 异常退出、ADB 超时、坏签名/hash、磁盘不足、端口占用和 frame producer 退出的故障注入；
10. 100 次启动停止、2 小时长稳、资源/句柄/任务/日志上限。

Windows 在线退出恢复由独立 runner 生成原始证据：

```powershell
.\scripts\windows-fault-injection.ps1 `
  -InstanceId <UUID> `
  -Output out\windows-fault-injection
```

它依次终止 frame producer、crosvm、Worker、Host，要求组件/VM 故障进入新 run 且 frame generation 递增，Worker 故障替换精确 identity，Host 故障则保留同一 Worker/run。每次恢复都重新验证 Android Ready 和严格 frame metrics，最终必须收敛到 Stopped。坏签名/hash、磁盘/CPU/内存准入、端口占用和 HTTP/IPC 错误仍由 capability、artifact、lease、security contract smoke 覆盖；Blocked 场景不得被 `on_failure` 自动重试。

平台 gate 生成原始证据文件，`xtask certify` 只在八类聚合证据名称精确匹配时签发最多 31 天的认证：`hd_quality`、`host_worker_smoke`、`http_security_smoke`、`lease_recovery_smoke`、`diagnostic_smoke`、`real_guest`、`zero_copy`、`device_profile_conformance`。认证不执行测试，也不能替代证据；它只把已经通过的证据 digest 绑定到当前 bundle/能力。

## 回读与判定

每轮必须回读：

- 每个命令退出码和 warning；
- required gate 是否全部存在且为 pass；
- `readback.json` 与任务卷 evidence state；
- 所有相关嵌套仓库 `git status --short` 和 diff check；
- run `manifest.json` 的 bundle/launch/toolchain，`events.jsonl` 的序号/边界/错误，`result.json` 的终态；
- Worker/Host descriptor 与进程身份、redb lease 和 cleanup_pending；
- 诊断 manifest/hash/脱敏结果；
- Windows PE imports；
- real-guest、zero-copy、device profile 的原始 runner 证据及签名有效期。

证据状态严格分层：代码存在、测试源码已编译、contract smoke 通过、真实 Guest 通过。前三项不能推出第四项；UI 按钮、协议类型、Blocked 行为或人工截图也不能推出真实功能通过。
