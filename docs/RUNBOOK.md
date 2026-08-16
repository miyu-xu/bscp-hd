# HD 运行与故障处理

## 启动与连接

发布目录中的十四个 HD exe、`ui/`、`WebView2Loader.dll`、crosvm/gfxstream、Microdroid `vm`/`virtmgr`/`libbinder-rpc`、Android `adb.exe`/`aapt2.exe` 及其 sidecar DLL 必须保持同目录。Windows GNU `hd.exe` 动态导入该 loader；发布包不得依赖开发机 PATH 中的 SDK/WPT/Android SDK/MinGW 副本，运行机仍需安装 Evergreen WebView2 Runtime。`hd.exe` 合并多实例 Manager 和 Player：选择实例即启动并在同一窗口附加原生 Android 显示；关闭 Player 只释放显示会话，实例继续运行，显式“关机/停止”才改变 Guest 生命周期。`hd.exe` 与不带 `--no-start-host` 的 `hdctl` 都会先验证现有 `host-runtime-v2.json`、进程身份及同目录 `hd-host` SHA-256；没有有效 Host 时拉起同目录 `hd-host.exe`。Host 在实例 start 时拉起同目录 `hd-worker.exe`。

安装新版本后，若旧 Host 没有活动实例，新客户端会自动关闭旧 Host 和空闲 Worker 并切换到当前包；若仍有活动实例，新客户端继续连接旧 Host，不会强制停止 Guest，界面持续提示“旧版 HD 运行时仍在承载实例”。停止所有实例并重新打开 HD 后完成升级。发布回归必须分别执行 `macos-host-runtime-upgrade-smoke.sh` 的空闲自动接管门，以及 `macos-host-runtime-active-upgrade-smoke.sh` 的真实 Microdroid 运行中延后门。

升级门必须先证明旧、新 `hd-host` SHA-256 不同；相同二进制不构成升级，必须在创建隔离 data root 或启动进程前拒绝。`--old-app`、`--target-dir` 和直接二进制输入不得是符号链接。`macos-host-runtime-upgrade-contract-smoke.sh` 固化相同摘要与符号链接负向合同，避免升级 runner 自身产生假阳性。

```powershell
hdctl health
hdctl capabilities
hdctl list
hdctl create --name Android-1
hdctl show <UUID>
hdctl resource-admission <UUID>
hdctl capabilities <UUID>
hdctl start <UUID>
hdctl action <UUID> key home
hdctl action <UUID> key recent
hdctl action <UUID> key back
hdctl action <UUID> rotate landscape
hdctl install <UUID> app.apk
hdctl diagnostics --instance-id <UUID>
hdctl stop <UUID>
```

`hd.exe --data-root <PATH>` 与 `hdctl --data-root <PATH>` 选择同一个隔离数据根；不传时使用 `%LOCALAPPDATA%\bscp\hd`。macOS 的 Finder/默认数据根启动保持单实例并激活已有窗口；显式 `--data-root` 表示独立 profile，可与默认 UI 并行用于发布 QA 和诊断。需要无界面后台实例时使用 `hdctl` 启动。`--no-start-host` 要求 CLI 只连接而不拉起 Host。没有活动实例时 `hdctl shutdown` 可退出 Host；存在活动实例时它会拒绝，必须显式使用 `hdctl shutdown --stop-all`。

## 创建可启动实例

默认实例没有 artifact selection，start 必然进入 `Blocked`。可启动 spec 必须通过 `hdctl update` 或 UI 设置精确选择：

- artifact store root；
- 64 位小写十六进制 Guest bundle digest；
- 64 位小写十六进制 Host bundle digest；
- CPU、内存、显示、ADB、boot 和 device profile。

store 布局：

```text
<store>/bundles/<digest>/manifest-v2.json
<store>/bundles/<digest>/READY-v2.json
<store>/bundles/<digest>/<manifest files>
```

不要手工编写 manifest/READY。取得固定构建产物和授权 Ed25519 密钥后，用 `xtask publish-bundle` 从明确的 `role=relative/path` 清单生成 bundle；命令先在同一 store 的 staging 目录完成逐文件 hash、签名、trust 验证和完整 resolver 复验，全部通过后才按 digest 原子发布：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- publish-bundle `
  --kind host-tools --input-root C:\artifacts\host --store-root C:\hd-store `
  --platform windows --architecture x86_64 `
  --source-manifest-digest <64_HEX> --signer-key-id release-2026 `
  --signing-key C:\keys\release.key --trust-store C:\hd-data\trusted-keys-v2.json `
  --capability hd-host-tools-v2 `
  --file crosvm=bin\crosvm.exe --executable-role crosvm
```

Guest 使用 `--kind guest --platform android`，并逐项登记 `kernel`、`initrd`、`rootfs`、`android_fstab` 及可选 `system_image`/`vendor_image` role。固定 Android 15 r14 profile 在 Windows/macOS 上通过 Guest 内置的 AOSP `cuttlefish_sensor_injection motion` 提供三轴姿态，不要求外置 `sensor-injector`；只有确实接入独立传感器 HAL 的其他 profile 才登记该可执行 role 并发布独立/定时注入能力。Host bundle 必须登记运行时要求的全部正式工具角色；缺一项时 resolver 会拒绝启动。

### 导入 Android x86_64 Cuttlefish 发布包

Google 标准 Cuttlefish image ZIP 不能直接作为 HD Guest bundle。Android 15 路径使用
同一 build 的 target-files ZIP 提供严格匹配的 vendor fstab。导入器不联网、不替换
输出、不读取密钥，也不执行签名、认证或 Guest 启动：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- import-cuttlefish `
  --image-zip D:\artifacts\aosp_cf_x86_64_phone-img-<BUILD>.zip `
  --target-files-zip D:\artifacts\aosp_cf_x86_64_phone-target_files-<BUILD>.zip `
  --sensor-injector D:\artifacts\guest\hd-sensor-injector `
  --output D:\artifacts\hd-staging\<BUILD>
```

对已经人工确认接受的 latest 构建，可提供官方 OTA metadata 文本；导入器会校验
build/SDK/x86_64/userdebug，并从 `vendor_boot.img` 的 legacy-LZ4 ramdisk 提取 fstab：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- import-cuttlefish `
  --image-zip D:\artifacts\aosp_cf_x86_64_only_phone-img-<BUILD>.zip `
  --ota-metadata D:\artifacts\aosp_cf_x86_64_only_phone-ota_metadata-<BUILD>.txt `
  --output D:\artifacts\hd-staging\<BUILD>
```

当前已验证的输入组合为 Android 15/SDK 35 和 Android 17/SDK 37 的 x86_64
userdebug Cuttlefish。
导入器拒绝 ZIP 路径穿越、symlink、重复关键镜像、缺失分区、错误 boot header、
错误 sensor-injector ABI 和已有输出目录。Android sparse `super`/`userdata` 会在
组盘前流式展开；发布包缺少 `metadata.img` 时会生成 64 MiB 可格式化空分区；输出
GPT 的 disk/partition GUID 由所有输入 digest 确定生成。

输出：

```text
<output>/guest/kernel
<output>/guest/initrd_android.img
<output>/guest/aggregate_android.img
<output>/guest/android_fstab.dt
<output>/guest/hd-sensor-injector       # 提供 --sensor-injector 时生成
<output>/import-manifest-v1.json
<output>/publish-request-v1.json
<output>/import-events.jsonl
```

`publish-request-v1.json` 只是后续发布参数底稿。输出明确保持
`production_ready=false`；只有转换后的 Guest 通过 real-guest、zero-copy 和
device-profile gate 后，人工才可授权其声明对应 Android capability、
`hd-guest-profile-v2`、`hd-device-bridge-v2` 并调用 `publish-bundle`。新 bundle
digest 还需要重新签发匹配的 Host certification。未提供 sensor-injector 的输出会带
`sensor_injector_missing` blocker。Android 17 输出会记录
`cuttlefish-android17-split-sensors-v1`（20 个 virtio-console，hvc18 控制、hvc19
数据）并带 `hd_runtime_android17_split_sensors_profile_not_integrated` blocker；在正式
runtime 和 Host bundle 接入该 profile 前禁止发布。

macOS Android 15 direct-boot 制品必须明确区分数据安全 profile。当前已验证的开发兼容
profile 使用 `--data-encryption none --fstab-suffix hd.direct` 生成；它会移除 `/data`
的 `inlinecrypt`、`keydirectory` 和 `fileencryption`，并将 `/data` 从 `latemount`
改为 `first_stage_mount`。原因是 HD 通过 crosvm Android DT 提供 fstab，而 Android
first-stage init 会完整读取 DT fstab；若 `/data` 仍为 late mount，它会在首阶段已挂载后
被二阶段再次挂载，`mount_all --late` 以 `EBUSY` 失败并阻止 `nonencrypted` 事件及
`installd` 启动。独立 `hd.direct` suffix 还可避免二阶段合并 product 中另一份不同安全
语义的 vendor fstab。

该 profile 必须标记 `android-data-unencrypted-development-v1`，不得声明静态数据加密，
也不能作为 production-security release。生产加密 profile 的解除条件是接入可跨重启
持久化的 hardware-backed KeyMint/secure environment，并重新通过首次启动、写入
`/data`、正常关机、第二次启动、数据回读、ADB、zero-copy 画面和外网门禁。仅验证
`sys.boot_completed=1` 的单次启动不足以发布。

Android 17 build `15885347` 的 Windows/WHPX 手工 smoke 结果如下：导入后的 GPT、
dynamic partitions、AVB/dm-verity、EROFS、metadata/userdata 格式化和 SELinux 均可工作；
补齐该版本的 multi-install vendor APEX selectors 后，`apexd` 成功扫描并激活 87 个
APEX，随后 adbd、zygote、音频、网络和多个 HAL 可以启动。因此发布 image ZIP 的
磁盘内容不需要重打包为另一套 Android 分区格式，但仍不能直接声明为 HD Ready。
当前 HD 正式 gfxstream guest-ANGLE/ranchu 路径在 SurfaceFlinger 初始化 virtio-gpu
context 时仍会出现 `DRM_IOCTL_VIRTGPU_GET_CAPS`/`DRM_IOCTL_VIRTGPU_CONTEXT_INIT` 错误和
WHPX soft-lockup。Windows 手工 smoke 已改用官方 Cuttlefish `guest_swiftshader` 合约：
crosvm `backend=2d`，Guest 使用 ANGLE/Pastel、minigbm 和 ranchu composer。为此 Windows
crosvm 允许显式 `Mode2D`，非 gfxstream 构建也补齐了显示窗口代码的 `anyhow::Context`
导入。build `15885347` 在该路径上已实测 `sys.boot_completed=1`、
`init.svc.bootanim=stopped`，SurfaceFlinger 识别 `CrosvmDisplay`，PackageManager 可查询
151 个包。转换后的分区无需重排，导入器生成的 Android 17 initrd 也固定采用这组
Guest bootconfig。

这条路径使用 Guest 软件 Vulkan，只证明发布镜像可转换并完整启动；它不满足 HD 正式
硬件渲染/严格零拷贝契约，也没有把 Android 17 split-sensors profile 接入正式 resolver，
因此不能据此移除 `hd_runtime_android17_split_sensors_profile_not_integrated` blocker 或
发布认证能力。

同一次诊断确认 `androidboot.openthread_node_id=1` 可消除 Thread HAL 的 `node_id=0`
退出；lights 还要求按实例 Guest CID 动态注入 `androidboot.vsock_lights_cid` 和
`androidboot.vsock_lights_port`，不能固化在可多实例复用的 initrd 中。对 build
`15885347` 对齐 `android17-release` 源码和官方 `launcher.log` 后，确认其布局不是旧版
`/dev/hvc13` 单通道，而是 `/dev/hvc18` sensors control 与 `/dev/hvc19` sensors data 两个
双向通道。Windows 20-console smoke 已完成两组命名管道连接、Sensors HAL 协议握手并注册
`android.hardware.sensors.ISensors/default`；完整启动后 SensorService 识别 3 个 Goldfish
硬件传感器并持续收到事件。ADB 已确认 Android 17/SDK 37、build `15885347`，
`sys.boot_completed=1`。此前的 rutabaga `-22`/SurfaceFlinger binder 阻塞已由 2D
Guest SwiftShader 路径关闭；剩余工作是正式 HD profile/resolver、动态 lights 参数、
sensor-injector、zero-copy 与 certification gate。

`cvd-host_package.tar.gz` 是 Linux host tools，不是 OTA metadata，也不是 HD Guest
bundle；Windows HD 不能直接执行其中的 ELF `launch_cvd`/`crosvm`。它可用于核对同一
build 的官方组装参数。在 Linux 主机上直接运行官方包仍要求 KVM 等受支持的 VM
manager；WSL 环境没有 `/dev/kvm` 时无法运行 Guest，但同 build 的 host package 可用于
生成并核对 assembly/launcher 配置。

不需要真实镜像即可执行独立结构 smoke：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- import-cuttlefish --self-check
```

data root 还必须有 `trusted-keys-v2.json`，包含受信 Ed25519 public key。Host 会验证签名、manifest/READY、逐文件大小/hash、平台/架构/capability、正式 component JSON probe，以及当前 bundle/能力对应的未过期签名认证。运行时不联网下载或补齐缺失内容。

在 start 前运行：

```powershell
hdctl capabilities <UUID>
hdctl diagnostics --instance-id <UUID>
```

只有 `certified=true` 且所有 required probe 和启用设备均 available 才具备启动资格。

## macOS 自包含应用、签名与公证

macOS 发布应用必须通过 `scripts/package-macos.sh` 从同一 release target、已构建 Web UI，以及明确的 Android/Microdroid 运行时输入生成。每个候选包还必须携带一个版本化 Microdroid conformance Payload；它不是用户实例的隐式默认工作负载，而是证明上传型 Payload、idsig 和 Guest Ready 链路与该版本兼容的发布证据。先从已经完成 APK Signature Scheme v3/v3.1 签名的 APK 生成严格 bundle：

```bash
./scripts/microdroid-payload-bundle.sh create \
  --apk /absolute/path/to/hd-minimal-payload.apk \
  --apksigner /absolute/path/to/android-sdk/build-tools/36.0.0/apksigner \
  --output-dir /absolute/path/to/hd-minimal-payload-1.0.0 \
  --artifact-id hd-minimal-payload \
  --version 1.0.0 \
  --channel release \
  --expected-signer-sha256 <release-payload-signer-certificate-sha256>
```

bundle 只包含 `payload.apk` 与 `payload-bundle-v1.plist`，清单固定记录版本、channel、大小、SHA-256 和实际 signer certificate SHA-256。使用原生 XML Property List 是为了让 release runner 只依赖 macOS 自带 `plutil`，不隐式要求 Python/jq。release 创建和验证都要求显式的期望 signer；development bundle 必须标为 `development`，不能混入正式包。脚本会调用 `apksigner verify`、要求唯一 signer、v3/v3.1 与唯一 `assets/vm_config.json`。

打包脚本不会复制完整的 Cuttlefish product。`microdroid-runtime-closure.sh` 从输入 product 生成可重现的 macOS arm64 v2 运行闭包：EmptyPayload APK、normal/debuggable initrd、microdroid.json、kernel、super、vbmeta，以及仅用于 Full debug 的原始 `com.android.adbd` APEX 与最小 APEX inventory，共 9 个运行文件，附带逐文件 SHA-256 与闭包元数据。该闭包对应 `empty_or_uploaded_with_debug_adbd_apex` 产品契约；不得静默从宿主 product 取其他 APEX，未来扩展必须再次升级闭包 schema 并执行真机认证。

development QA 可以通过 `--android-product` 显式输入 `products/android/vsoc_arm64_only`，也可以通过 `--android-artifact-store` 输入完整的 `signed-artifact-store-v2`，但两者严格互斥。直接镜像模式只复制 `direct-linux` 的 kernel、initrd、aggregate 和 fstab，要求文件不是符号链接，要求 `/data` 唯一、无加密标志且使用 `first_stage_mount`，并生成 `runtime-files-v1.sha256`。签名仓库模式则验证 `index-v2.json`、包内 trust store、Guest/Host Ed25519 签名、逐文件摘要、相对路径、fstab/data-profile 与精确闭包，再用 APFS clone 安装到应用；任一父目录符号链接或额外文件都会阻断。正式包禁止 `--android-product`，强制使用 channel 为 `release`、data profile 为 `metadata-encrypted` 且由生产信任根验签的 artifact store。持久化硬件 KeyMint 与对应认证仍是对外安全声明的独立阻塞项，不能把 development-unencrypted 闭包改名为 release。

签名仓库在打包前必须独立复验；trust store 路径必须来自同一受控发布输入，不能从用户 data root 或网络下载后直接信任：

```bash
./target/release/xtask verify-android-artifact-store \
  --store-root /absolute/path/to/android-artifact-store \
  --trust-store /absolute/path/to/android-artifact-store/trusted-keys-v2.json \
  --channel development

./scripts/macos-android-artifact-store-smoke.sh \
  --xtask /absolute/path/to/target/release/xtask \
  --store /absolute/path/to/android-artifact-store \
  --output /absolute/path/to/fresh-store-contract-evidence
```

发布 runner 不得依赖交互式 shell 的 PATH、系统 Java、旧 `node_modules` 或旧 `web/dist`。`macos-release-toolchain.sh` 固定校验 Node 22.23.1/npm 10.9.8、Temurin 21.0.12+8 arm64、Android build-tools 36.0.0 及其发布包/关键运行文件摘要，然后在隔离目录执行干净 `npm ci` 和 production build。源输入清单、dist 清单和工具链身份共同写入 evidence：

```bash
./scripts/macos-release-toolchain.sh build-web \
  --node-root /absolute/path/to/node-v22.23.1-darwin-arm64 \
  --node-archive /absolute/path/to/node-v22.23.1-darwin-arm64.tar.gz \
  --java-home /absolute/path/to/temurin-21.0.12+8/Contents/Home \
  --java-archive /absolute/path/to/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz \
  --android-build-tools /absolute/path/to/android-sdk/build-tools/36.0.0 \
  --web-root /absolute/path/to/hd/web \
  --output /absolute/path/to/fresh-web-dist \
  --evidence-dir /absolute/path/to/fresh-toolchain-evidence
```

打包脚本把 Host/Worker、正式设备组件、crosvm、Microdroid vm/virtmgr、上述最小运行闭包、conformance Payload、gfxstream/ANGLE/Vulkan 运行库、ADB、aapt2 和网络兼容服务安装脚本一并封入 `HD.app`；development 包还封入经过上述检查的 Android 直接启动闭包。所有 Mach-O 逐个签名，crosvm 保留 Hypervisor/JIT entitlement，应用生成使用相对路径的逐文件 SHA-256 清单。工具链证据安装到 `Contents/Resources/provenance/toolchain`；`Contents/Resources/release` 只允许生产信任根与认证，development 包不得用普通构建证据触发正式信任材料安装语义。输出路径必须不存在，避免把旧 dylib、Guest 镜像或 Payload 混入新版本：

```bash
./scripts/package-macos.sh \
  --target-dir ./target/release \
  --runtime-dir /absolute/path/to/out/dist/macos \
  --microdroid-product /absolute/path/to/products/microdroid/vsoc_arm64_only \
  --android-product /absolute/path/to/products/android/vsoc_arm64_only \
  --web-dist ./web/dist \
  --adb /absolute/path/to/adb \
  --aapt2 /absolute/path/to/aapt2 \
  --apksigner /absolute/path/to/apksigner \
  --node-root /absolute/path/to/node-v22.23.1-darwin-arm64 \
  --node-archive /absolute/path/to/node-v22.23.1-darwin-arm64.tar.gz \
  --java-home /absolute/path/to/temurin-21.0.12+8/Contents/Home \
  --java-archive /absolute/path/to/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz \
  --android-build-tools /absolute/path/to/android-sdk/build-tools/36.0.0 \
  --release-toolchain-evidence /absolute/path/to/fresh-toolchain-evidence \
  --microdroid-payload-bundle /absolute/path/to/hd-minimal-payload-1.0.0 \
  --microdroid-payload-signer-sha256 <release-payload-signer-certificate-sha256> \
  --output /absolute/path/to/release/HD.app \
  --identity "Developer ID Application: Example (TEAMID)" \
  --release-materials /absolute/path/to/release-materials \
  --notary-profile hd-notary \
  --version 0.1.0 --build 1
```

本地 RC 必须显式传入 `--development-package`，并在 `--android-product` 与 `--android-artifact-store` 中选择一个，同时提供 channel 为 `development` 的 Payload bundle；此时可以使用 ad-hoc 应用签名且不要求生产发布材料，但不能对外发布。正式发布必须提供只读发布材料目录（`trusted-keys-v2.json` 与平铺的 `certifications/*.json`；不接受符号链接、子目录或其他文件）、release Android artifact store、release Payload signer 摘要和 channel 为 `release` 的 Payload bundle，由 Apple Developer ID Application 证书签名，`notarytool` 返回 Accepted，成功 staple，并通过 `spctl --assess`。正式包不得携带 `development-direct-v1.plist`。Host 在改动用户目录前先确定性地读取并验签全部包内认证；认证逐文件原子安装，首次信任根最后安装，因此损坏包不会留下“有信任根、无有效认证”的半安装状态。升级绝不替换既有信任根；同一身份、同一签名者且签发时间更晚并延长有效期的认证可续期，身份冲突或有效期未延长则阻断。每个自动启动请求使用独立 UUID 错误记录；Host 首启失败会立即回传稳定错误码，Finder 启动的 UI 以原生对话框展示错误并可打开日志目录。全隧道 VPN 下的 Guest NAT 还要求安装包以管理员权限执行应用内 `Contents/Resources/scripts/macos-network-setup.sh install`；仅把脚本放入 `.app` 不等于已安装系统服务。

压缩分发不能只验证归档能打开。自包含 Android aggregate 是逻辑 16.5 GiB、物理约 1.6 GiB 的稀疏文件；macOS ZIP 解包会把空洞写成真实零块，因此会额外占满约 16.5 GiB，不能作为该候选的发布格式。`macos-release-distribution.sh` 对自包含 Android 强制使用 BSD tar 的 sparse 扩展与 xz 压缩；旧 ZIP 仅兼容不含稀疏 Android aggregate 的历史包。脚本先验证源应用的相对路径清单、deep codesign、Microdroid 闭包、Payload signer、Web 工具链身份，以及 development Android 闭包的四文件清单与 fstab 安全配置，再创建不含 Finder/xattr 噪声的 `tar.xz`；随后解包到新位置并重复全部验证，同时确认稀疏文件没有被稠密展开。输出包含 archive SHA-256、可移植应用树清单和 `distribution-v2.plist` 身份 sidecar；sidecar 记录归档格式、Android 数据配置与 aggregate 摘要，防止把开发无加密镜像误当生产镜像。应用与归档 checksum/metadata 旁车均先写入同目录私有临时文件并用原子替换发布；中断不得留下可被误认为有效的 0 字节或半写入旁车：

```bash
./scripts/macos-release-distribution.sh create \
  --app /absolute/path/to/HD.app \
  --app-checksums /absolute/path/to/HD.app.sha256 \
  --archive /absolute/path/to/HD-0.2.0-release-macos-arm64.tar.xz \
  --node-root /absolute/path/to/node-v22.23.1-darwin-arm64 \
  --node-archive /absolute/path/to/node-v22.23.1-darwin-arm64.tar.gz \
  --java-home /absolute/path/to/temurin-21.0.12+8/Contents/Home \
  --java-archive /absolute/path/to/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz \
  --android-build-tools /absolute/path/to/android-sdk/build-tools/36.0.0
```

development 归档的真实 Guest QA 必须从解包后的应用启动，并显式为 Host 设置 `HD_MICRODROID_DEV_BYPASS=1`；Empty Payload 与版本化 uploaded Payload 都要到达 Ready。`debug=full` 时两者还必须通过包内 ADB 在独立回环端口执行有界 shell 回读，`debug=none` 必须没有 ADB serial，再完成 stop/delete/shutdown 无孤儿清理。正式归档禁止该变量，必须依赖随包签名的信任根与认证。

安装包级 Guest 门禁使用同一条可重复命令；它先调用 distribution verifier，在独立目录解包，再只使用包内二进制、运行闭包和 Payload 完成两类 Guest 生命周期：

```bash
./scripts/macos-microdroid-distribution-smoke.sh \
  --archive /absolute/path/to/HD-0.2.0-development-macos-arm64.tar.xz \
  --output /absolute/path/to/fresh-installed-guest-evidence \
  --node-root /absolute/path/to/node-v22.23.1-darwin-arm64 \
  --node-archive /absolute/path/to/node-v22.23.1-darwin-arm64.tar.gz \
  --java-home /absolute/path/to/temurin-21.0.12+8/Contents/Home \
  --java-archive /absolute/path/to/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz \
  --android-build-tools /absolute/path/to/android-sdk/build-tools/36.0.0 \
  --development-package
```

同一候选的 Android 门禁不接受“应用目录曾经启动过”替代安装分发验证。runner 会再次验证并解包归档，以不注入外部 Android、ADB、aapt2 或 gfxstream 路径的桌面壳连续启动两次 Android 15，验证 ADB、网络、userdata 标记持久化、单调 frame generation、非录制显示零拷贝能力和无孤儿清理。首次 Ready 后还必须用 `hdctl bugreport <instance-id>` 生成完整 AOSP dumpstate ZIP，复验当前 instance/run、0600、22 字节至 256 MiB、SHA-256、主 bugreport 文本成员和产物后 ADB Ready；它包含敏感数据，不得作为普通脱敏诊断包直接分享。录屏门会显式启动 macOS gfxstream recording-only readback + AVFoundation 硬件 H.264 writer，驱动画面、停止并验证 MP4 顶层盒、样本数、墙钟/媒体时长边界和摘要；视频以 0600 权限进入证据目录，测试生成的用户 `Movies/HD` 文件必须删除。录制结束后继续执行旋转与第二次启动，因此不能用 Guest `screenrecord` 或一次性成功文件掩盖显示状态损坏。启用 Bluetooth 的候选必须生成独立 `macos-bluetooth-real-guest` 门：确认包内正式 AOSP RootCanal、Guest 非零 H4、虚拟 GATT peer 创建/广告开关/移除动作，以及动作后和第二次启动后的 Bluetooth HAL、framework binder 与 `ON` 状态全部存活。启用 UWB 的候选必须生成独立 `macos-uwb-real-guest` 门：确认包内正式 FiRa v2 UCI component、Guest 非零 UCI，以及首次和第二次启动后的 UWB HAL、AIDL/framework service、`READY` 设备状态与国家码。启用 NFC 的候选还必须生成独立 `macos-nfc-real-guest` 门：确认包内正式 AOSP Casimir、Guest 非零 NCI、Type 2/Type 4/移除动作，以及动作后和第二次启动后的 NFC HAL、Cuttlefish HAL 进程与 framework binder 全部存活。启用 Modem 的候选必须生成独立 `macos-modem-real-guest` 门：确认包内正式 modem component、owner-only Guest-CID host-vsock UDS、vendor RIL/Radio HAL/telephony framework，以及测试运营商 `00101` 在两次启动均成立。失败证据原子保留在 `<output>.failed`；development-unencrypted 与缺少硬件 KeyMint 会明确记录为正式发布阻塞而不是伪装成生产安全通过：

```bash
./scripts/macos-android-distribution-smoke.sh \
  --archive /absolute/path/to/HD-0.2.0-development-macos-arm64.tar.xz \
  --output /absolute/path/to/fresh-installed-android-evidence \
  --node-root /absolute/path/to/node-v22.23.1-darwin-arm64 \
  --node-archive /absolute/path/to/node-v22.23.1-darwin-arm64.tar.gz \
  --java-home /absolute/path/to/temurin-21.0.12+8/Contents/Home \
  --java-archive /absolute/path/to/OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.12_8.tar.gz \
  --android-build-tools /absolute/path/to/android-sdk/build-tools/36.0.0 \
  --location-probe-apk /absolute/path/to/hd-location-probe.apk \
  --zstd /absolute/path/to/zstd \
  --development-package
```

Host 首启故障注入必须使用 release 构建产物并验证两个并发客户端均在五秒预算内得到 `release_materials_invalid`，且没有部分信任状态、错误记录或进程泄漏：

```bash
./scripts/macos-host-startup-negative-smoke.sh \
  --target-dir /absolute/path/to/target/release \
  --evidence-dir /absolute/path/to/fresh-startup-negative-evidence
```

Microdroid 发布身份必须从最终闭包生成。release runner 先独立生成并验证闭包，再生成认证身份；打包时会从同一 product 重新生成闭包和身份，输出必须得到同一对 digest：

```bash
./scripts/microdroid-runtime-closure.sh create \
  --product-root /absolute/path/to/products/microdroid/vsoc_arm64_only \
  --output-dir /absolute/path/to/microdroid-runtime-closure-v2

./scripts/microdroid-runtime-closure.sh verify \
  --closure /absolute/path/to/microdroid-runtime-closure-v2

./scripts/microdroid-release-identity.sh \
  --runtime-dir /absolute/path/to/out/dist/macos \
  --product-root /absolute/path/to/microdroid-runtime-closure-v2 \
  --output-dir /absolute/path/to/identity

cargo run -p xtask -- certify \
  --guest-kind microdroid \
  --data-root /absolute/path/to/release-materials \
  --guest-digest <guest_digest> \
  --host-digest <host_digest> \
  --capability-fingerprint <dry-run-evidence_fingerprint> \
  --signer-key-id <release-key-id> \
  --signing-key /offline/path/to/release-signing.key \
  --evidence \
    hd_quality=<file> \
    host_worker_smoke=<file> \
    http_security_smoke=<file> \
    lease_recovery_smoke=<file> \
    diagnostic_smoke=<file> \
    microdroid_real_guest=<file> \
    microdroid_multi_instance=<file> \
    microdroid_payload_conformance=<file>
```

Microdroid 启动还要求与当前包和平台匹配的认证。未签发认证时保持 Blocked，不得由发布包自动降级。仅限隔离开发验证时，可以从 Terminal 为 Host 显式设置 `HD_MICRODROID_DEV_BYPASS=1`；该变量代表未认证运行，不能写入应用包、启动项或发布脚本。CPU 拓扑默认为可并行调度的 `one_cpu`；显式选择 AOSP `match_host` 时，Host 必须把全部逻辑 CPU 作为独占租约，只有当前不存在任何 CPU 租约时才能启动，并在该 VM 停止前拒绝其他 Android/Microdroid 实例启动。`cpu_count` 对 Microdroid 继续固定为兼容哨兵值 1，实际拓扑只由 `microdroid.cpu_topology` 决定。

上传型 Microdroid 主 Payload 可在停止状态下按 `assets/vm_config.json` 的 `extra_apks` 顺序添加最多 8 个额外 APK。每次添加都先验证普通 APK 结构和 v3/v3.1 signing block，再进入 owner-only 上传仓库；实例规格仅记录 upload UUID/SHA-256。移除或调整顺序后必须保存并重启；更换主 Payload 会自动清空旧列表，避免把上一份配置的身份集合误用于新工作负载。启动时 `vm --extra-apk-override/--extra-idsig` 成对出现且数量必须与签名配置精确相等，virtmgr 只使用调用方已经打开的文件描述符，不读取配置中的任意宿主路径。extra APEX 仍不支持。

## 生命周期语义

- `start`、`stop`、`restart`、`pause`、`resume`、`delete`、`display`、`install` 和 diagnostics operation 都持久化，可用 `hdctl operations` / `hdctl operation <OP_UUID> --wait` 回读。
- 重复的内部 idempotency key 不会创建第二个操作；CLI 默认等待，`--no-wait` 返回 operation 后立即退出。
- Graceful stop 按 Guest 类型发送固定关机动作并等待配置时限，再精确终止其余 Host 组件；`--force` 直接终止。Android 继续使用 Cuttlefish shell policy 已允许的 `adb shell reboot -p`，不得为了与 Microdroid 共用代码而执行 `adb root` 或改变 Android adbd 权限状态。
- Microdroid 只有在 Payload 的 ADB 通道已经 Ready 时才允许 graceful stop/restart；Full debug 的固定关机链路先检查 adbd 身份，必要时按 AOSP 合约执行 `adb root` 并等待实例回环桥恢复，确认 `uid=0` 后才发送 `reboot -p`。这避免把 `adb shell reboot -p` 错误地放进无权写 `sys.powerctl` 的 shell SELinux 域；门禁必须回读 `reboot: Power down` 且没有强制终止。ADB 未 Ready 时返回 `unsupported`，UI 只提供明确标注的强制停止。已有 `storage.img` 不支持原地调整容量，容量不匹配会阻断启动。
- Windows Full-debug 使用 product 工厂 APEX 扫描补齐桌面不存在的 `apex-info-list.xml`，并通过 overlapped 全双工命名管道桥接 Guest vsock 与实例回环 ADB。CAPEX 解压文件位于每 run 临时目录，但 Payload 身份必须使用原始 product 文件时间戳并按模块名/预安装路径稳定排序；否则同一实例第二次启动会被 Guest 以 `APEXes have changed` 拒绝。发布回归至少连续启动同一实例三次，每次都要回读 SDK、Guest power down 和零残留。Windows Full-debug/64 MiB 存储三轮开发证据为 `hd/out/ai/windows-microdroid-full-storage-20260809-r2/result.json`，None-debug/无 ADB 证据为 `hd/out/ai/windows-microdroid-none-20260809-r5/result.json`。
- Windows Player 的解锁桌面输入、关闭和重开门由 `windows-real-guest.ps1 -RunActions -SecondaryDisplayCount 1 -RunUiDisplayInput` 执行。`hd/out/ai/windows-android-ui-input-20260809-r6/windows-real-guest.json` 已证明物理 `SendInput` 到 Guest X/Y 与 DOWN/UP、render/input 同时停放、同一对 HWND 重挂、新旧 UI 均从原生关闭按钮退出、Ready run 不变且残留为 0。若命中 `LockScreenBackstopFrame`，该门继续失败而只保存直达 render 的诊断证据。
- 有限 Microdroid Payload 可能在 Host 第一次观察 Ready 之前完成。Worker 必须等待 `vm` 启动器退出后最多 2 秒让继承 stdout/stderr 的后代关闭句柄，再同时验证唯一 `payload finished with exit code N`、唯一 `VM ended: Shutdown` 和启动器退出 0；Guest console/trace 不得替代这三项。启动操作仍持有实例操作锁时直接调用 locked stop cleanup，不能通过异步 monitor 重新获取同一锁。返回 0 必须是 `Stopped / exit_code=0`，非零必须是 `Failed / microdroid_payload_failed / 原始退出码`，二者都要清除 VM/component/endpoint/租约。Windows 材料由 `scripts/windows-microdroid-finite-payload-materials.ps1` 生成，只保留 v3 APK 与开发公钥证书，不保留临时私钥或 v4 idsig。
- Mac 真 Guest 门禁使用 `scripts/macos-microdroid-finite-payload-smoke.sh`。输入必须是最终 `HD.app`、独立验证过的 arm64 exit 0/17 v3 APK、包内渠道匹配的 `--development-package` 以及固定 Android build-tools；输出目录必须不存在。脚本为两个 case 分配独立实例，只轮询 `Stopped`/`Failed` 而不等待 Ready，逐项验证受管 upload、Full-debug `run-app` manifest、严格双标记、Guest 完成通知、类型化 `result.json`、自然清理和零 VM 残留。它只终止命令行包含自身随机 data-root 的进程，不得复用生产 data-root 或清理其他实例：

  ```sh
  scripts/macos-microdroid-finite-payload-smoke.sh \
    --app /absolute/path/to/HD.app \
    --output /absolute/path/to/fresh-evidence \
    --exit-0-payload /absolute/path/to/microdroid-finite-exit0-arm64-v3.apk \
    --exit-17-payload /absolute/path/to/microdroid-finite-exit17-arm64-v3.apk \
    --apksigner /absolute/path/to/apksigner \
    --zipalign /absolute/path/to/zipalign \
    --development-package
  ```
- macOS development build140 的最终 tar.xz 已由 `hd-macos-arm64-installed-microdroid-v3` 门禁独立解包验证：五个 Full-debug 停止场景均为 Guest power down、forced termination 为 0，Empty/Uploaded ADB shell 均回读 SDK 35，`None` 保持无 ADB，并发隔离和加密存储重启复用通过。该结果只覆盖 development bypass，不替代生产签名、notary、生产信任根与硬件 KeyMint 门禁。
- `shutdown --stop-all` 是部署、升级和测试清理边界：没有 Ready ADB 的 Microdroid 直接使用 force；其他 Guest 先 graceful，失败时记录 `host.shutdown.force_fallback` 并强制回收，随后才关闭空闲 Worker 与 Host。`hdctl` 等外部进程客户端只有在原 Host 的精确 PID/启动标记已经退出并释放数据根锁后才返回成功；接受关闭后五秒仍未退出会明确返回 `HostShutdownTimeout`，后继命令不得遭遇短暂的 `host.lock` 竞态。嵌入 Host server 的调用方提交 `shutdown` 后自行 join 所属 server task，不能等待承载它的同一进程退出。它不得因单个 Guest 不支持 graceful stop 而留下整组后台进程。
- Microdroid Payload 在导入和 Worker 启动前必须同时通过 ZIP 结构、唯一 `assets/vm_config.json` 与 APK Signature Scheme v3/v3.1 signing block 预检；缺少 v3 的 APK 不会保存为实例设置，也不会等到 Guest 启动后才暴露为模糊失败。Guest 仍执行完整密码学验签，预检不能替代 AVF 信任边界。
- 自动化或 CLI 导入使用 `hdctl upload --microdroid-payload /absolute/path/to/payload.apk`，成功结果中的 `id` 与 `sha256` 写入该实例的 uploaded Payload 配置；不带该标志的普通上传仍可服务 Android APK，不能据此绕过 Worker 的 Microdroid 二次预检。
- `Stopped` 意味着 child 不存在、endpoint 已清理、`cleanup_pending=false`。若清理失败则保持 Failed 和租约，不能手工删 lock/pipe 冒充修复。
- 每次实例启动前会维护该实例的已完成 run：超过 64 MiB 的 `.log`/`.txt` 仅保留最后 16 MiB 到 owner-only `.previous`，随后最多保留最近 20 轮且总逻辑大小不超过 2 GiB；即使超过预算也至少保留最近 5 轮用于诊断。没有最终 `result.json` 的活动或异常现场不会被自动压缩或删除。压缩与删除分别记录 `runtime.run.log_compacted`、`runtime.run.pruned`，无需也不应手工清理正在运行的目录。
- `restart_policy=on_failure` 会处理两类在线故障：组件/crosvm 失败时复用已清理的 Worker 启动新 run；精确 Worker identity 死亡时释放旧租约并创建替代 Worker。自动恢复最多连续尝试 3 次，同一失败 revision 只排队一次；达到 Ready 或用户 stop 后计数清零。Blocked 不自动重试。
- Host 异常退出不终止 detached Worker/VM。新 Host 必须通过 descriptor/endpoint/secret 和 PID+启动标记认证后重连同一 run；不能把 Host 重启误实现为 Guest 重启。
- 删除仅允许非活动实例；它删除实例配置、私有磁盘、Worker 数据和该实例全部 run history，不可恢复。先生成诊断并另行归档所需证据。

## 常见阻塞

### `release.certification` 或 `artifact.*` Blocked

读取 `hdctl capabilities <UUID>` 中每个 probe 的 detail/properties。常见原因是 trust store 缺失、bundle digest/签名/READY/hash 不匹配、Host bundle 平台不符、正式 component probe 失败、认证与能力指纹不匹配或过期。不要复制另一平台认证或修改 JSON；重新生成对应 runner 证据并由授权密钥签发。

### `host.resources` Blocked

probe 同时列出 logical/requested CPU、其他实例已预留和当前剩余 CPU、total/available/requested memory、Host reserve、其他实例内存预留、memory source、可用磁盘和 `disk_requirement_mode`。新实例或尚未分配私有存储的实例按完整配置容量执行 `new_instance_storage` 准入；已经存在 regular、非符号链接且尺寸兼容的私有存储时，重启按 1 GiB `existing_instance_storage` 运行余量准入，不能再次重复预留完整磁盘。该优化不跳过 `DiskProvisioner` 的类型、链接和精确尺寸校验，损坏或尺寸错误的存储仍会阻断启动。轻量预检排除当前实例自己的租约，并把其他实例的活动租约计入结果；`match_host` 发现任一其他 CPU 租约时直接显示独占冲突。start 仍会在同一 redb 事务中汇总全部活动实例的 CPU/内存租约，避免预检后的并发变化越限。降低实例规格或停止其他实例以释放租约；不能绕过 reserve。macOS 使用总内存 75% 的确定性安全预算。

实例设置的“性能”页直接显示最近一次 `host.resources` 结果，包括请求/扣除其他活动实例租约后的当前可用 CPU、包含 Host reserve 的内存预算、可用/所需磁盘和存储模式；“重新检测”通过轻量 `/resource-admission` 路径只读取已保存规格、当前 CPU/内存/磁盘和 redb 租约快照，不加载信任材料、不哈希 Android bundle、不运行设备 component probe，也不修改配置或启动实例。Player 选择和新建实例只自动走这条快路径；完整设备能力只在进入设备界面时按需发现，避免在严格 start 前重复读取 16.5 GB aggregate。CLI 可用 `hdctl resource-admission <UUID>` 复现同一结果。预检只用于解释，真正启动仍以原子租约事务为准。

该快照用于解释而不是授权，真正 start 仍重新执行严格发现、完整签名/逐文件哈希和原子租约准入。若 start 被阻断，`capability_blocked` 必须携带具体 probe、资源数值与存储模式，UI/CLI 不得只显示笼统的“能力不可用”。资源准入已经阻断时，制品、设备和认证检查尚未完整执行，其派生失败必须折叠为 deferred 说明，不能与 `host.resources` 根因并列制造噪声。资源刷新使用独立代次；切换实例或保存设置期间返回的旧响应必须丢弃。

资源 probe 已明确阻断时，Host 不再先哈希大型 Android bundle 或启动设备 component probe，而是记录 `capability.artifacts.deferred`，使能力页和 start 快速返回。资源恢复后的下一次严格 start 会重新执行完整 bundle 签名、逐文件哈希、zero-copy 和设备探测，不能把 deferred 当作已验证。用 `scripts/macos-host-resource-admission-smoke.sh` 在隔离 2 GiB APFS 卷复验；不要创建大占位文件或填满用户数据卷。

桌面壳的常规状态刷新只调用 `GET /v2/ui-snapshot`。Host 从一次持久化枚举同时生成实例摘要和选中详情，避免生命周期变化期间由 list/show 两个请求拼出不同 revision。命令执行或过渡状态每 250 ms 刷新；稳定前台 Player 每 1 秒，稳定设置/设备页和失焦窗口每 2 秒，最小化窗口每 5 秒。事件循环使用原生 `WaitUntil` 定时，不应存在独立 100 ms ticker。该快照只读取实例存储，不执行 capability、制品哈希、外部工具或设备探测；如果状态更新延迟，先检查窗口焦点和实例是否处于稳定态，不要恢复固定 4 Hz 双请求轮询。

恢复出厂数据只在 Android 实例为 Defined/Stopped 时开放。确认对话框要求逐字输入实例名，Host 还会复验 snapshot revision；旧 userdata 会移动到私有 powerwash 备份而不是立即删除。存在备份时不能再次 powerwash：可选择“恢复旧数据”（当前 userdata 自动变成新的回滚备份）或再次输入实例名后“永久丢弃备份”。不要手工移动、改名或删除 `disks/powerwash` 文件；如果 Host 报 `powerwash_recovery_ambiguous`，保留 overlay/source/rollback 原状并收集 Host-only 诊断，不能用删除其中任意一个文件来解除 Blocked。

macOS 桌面壳不得在 winit event handler 安装前创建 NSWindow、NativeDisplayHost 或 WebView，否则每次 Finder 启动都会向 UI 日志写入 `tried to run event handler, but no handler was set`。窗口初始化必须发生在首次 `Resumed`，重复恢复不得创建第二套界面；初始化失败必须继续进入原生 fatal dialog。发布候选用 `scripts/macos-ui-native-lifecycle-smoke.sh --app <HD.app> --output <fresh-dir>` 通过 Finder 启动复验，并确认隔离 Host 认证关闭和零残留；不要通过关闭 winit 日志目标隐藏错误。

### `display.zero_copy` Blocked

Host bundle 的 `frame-producer --probe-v2 --json` 没有证明当前平台要求的 external memory、explicit sync、same adapter 和 validation clean。HD 没有软件 blit/readback 回退。更新驱动/选择同一 GPU/换用已认证 Host bundle，然后重新产生证据。

### start 到 `NegotiatingDisplay` 后失败

检查该 run 的 `frame-ready-v2.json`、crosvm stderr 和 events。marker 必须绑定相同 instance/run/generation/transport，producer PID+启动标记仍存活，并证明严格零拷贝。超时、旧 generation 或 producer 退出都会失败并触发精确清理。

### `AdbConnecting` 失败

确认租约 port 未被其他进程占用、ADB 只连接 `127.0.0.1:<port>`、Guest adbd service/bridge 与同一 CID 对应。查看 `adb connect`、wait-for-device、boot property 和 package-manager probe 错误。HD 不尝试公网地址或系统代理。

### macOS Guest 有网关但不能访问外网

macOS 上的 socket_vmnet shared NAT 在 Host 使用全隧道 VPN 时，Guest 数据包可能被原样
路由到 `utun`，VPN 无法为 `192.168.105.0/24` 回程。设备页会以普通用户权限读取网络
服务状态；`ready` 表示当前链路已就绪，`maintenance` 表示网络仍可用但服务需要安装、
升级或修复，`degraded` 表示当前 VPN 出口缺少有效 NAT，`offline` 表示 socket_vmnet
数据面不可用。安装包版本与系统 helper 通过 SHA-256 绑定，不以“文件存在”冒充版本一致。

设备页的“安装/升级/修复网络服务”会先解释变更范围，再触发 macOS 原生管理员授权。安装
过程先保存 PF、helper、LaunchDaemon 和 PF enable token 的旧状态；任一步失败都会恢复旧
文件、重新加载旧服务和 PF 配置。检测到符号链接、非普通文件或其他不安全系统路径时不会
自动覆盖，状态进入 `manual_repair`。管理员进程不直接执行用户可写 `.app` 路径中的脚本；
HD 先要求包内资源与编译进已签名 Mach-O 的脚本逐字节一致，再由管理员进程把内嵌快照
解码到 root 私有临时目录执行并在结束时清理，避免校验与执行之间的替换竞态。开发环境也
可明确执行：

```bash
sudo ./scripts/macos-network-setup.sh install
./scripts/macos-network-setup.sh status
```

服务只在 `/var/run/socket_vmnet` 存在且 Host 默认出口是 `utunN` 时，在独立 PF anchor
中为固定 Guest 子网增加 NAT；VPN 关闭、出口变化或 socket 消失时自动清理规则。它不
修改 Android 内部分辨率、ADB、流量整形或 Host 的默认路由。卸载使用
`sudo ./scripts/macos-network-setup.sh uninstall`，会移除 LaunchDaemon、helper 和 PF
引用，并保留安装前 `/etc/pf.conf` 的 root-only 备份。

发布门禁使用 `scripts/macos-network-product-smoke.sh` 从源码和最终 `.app` 各执行一次
非特权状态读取，校验严格字段集合、状态一致性、源码/安装摘要以及事务回滚契约；它不会
修改 `/Library`、launchd 或 PF。管理员安装属于用户明确触发的系统变更，不能在启动或
打包时静默执行。应用启动、Player、设置、诊断、失焦和最小化状态都不会启动网络 status
进程；只有设备页可见并获得焦点时才按 30 秒缓存上限刷新。进入设备页、手动刷新和安装
完成会按契约触发检查，离开设备页会使旧响应失效且不会在后台补发。UI 中的状态读取有
3 秒截止时间；每次检查都进入独立进程组，超时或父
进程提前退出时会回收完整父子进程树。管理员安装会使已发出的旧代次状态响应失效，安装
期间不启动新轮询，避免旧结果覆盖安装后的真实状态。UI 契约门禁还会以 8 路并发读取和
真实父 shell/子进程挂起命令验证这些边界，并调用与生产安装相同的命令生成器，以 `status`
动作真实解码并执行内嵌快照，确认 staging 零泄漏、篡改资源被拒绝，并用 `osacompile`
编译完整管理员授权表达式；该检查不弹授权框、不执行 `install`。

`hdctl diagnostics --instance-id <UUID>` 的 `guest.network` 以 Android
ConnectivityService 的 active default network、DNS、默认路由和 `VALIDATED` 为准。
socket 文件存在只表示数据面可以连接，不再作为 Guest 已联网的证明。

### Home/APK/设备动作被拒绝

动作要求状态为 Ready。APK 还要求上传 hash 与 Worker 重新计算一致，并在安装后从 package manager 回读包路径。蓝牙/NFC 要求对应正式 adapter 控制面；不存在时返回稳定 device adapter 错误，不会本地伪造成功。

### macOS 蓝牙控件成功但 Android Bluetooth 未开启

先检查 `bluetooth-lifecycle.tsv`，并通过包内 ADB 回读 `pidof android.hardware.bluetooth-service.default`、`service check bluetooth_manager` 和 `dumpsys bluetooth_manager`；同时确认 RootCanal component 已发布 ready、进程仍存活且 Guest H4 输出文件大小大于零。启用 Bluetooth 的期望状态是 HAL PID 非空、binder `found`、`enabled: true` 且 state `ON`，虚拟 peer 创建、广告开关、移除动作后及第二次启动都必须重复成立。

运行时设备策略负责执行 framework enable；不要用手工 `svc bluetooth enable`、重启 Guest 或软件 capability 占位掩盖生命周期错误。复现使用 `scripts/macos-android-distribution-smoke.sh`，保留 `result.json`、`bluetooth-lifecycle.tsv`、H4 字节计数、动作响应和 `installed-android-gates.json`。当前正式边界是 AOSP RootCanal 确定性虚拟 BLE peer，不代表物理 RF。

### macOS UWB 显示可用但设备状态不是 READY

先检查 `uwb-lifecycle.tsv`，并通过包内 ADB 回读 `pidof android.hardware.uwb-service`、`service list` 与 `dumpsys uwb`；同时确认 UWB component 已发布 ready、进程唯一存活且 Guest UCI 输出文件非零。启用 UWB 的期望状态是 HAL PID 非空、`android.hardware.uwb.IUwb/default` 和 framework `uwb` service 同时存在、`Device state = READY` 且国家码为实例配置值，第二次启动必须重复成立。

不要用只启动 HAL、软件 capability 或 Host Ping 冒充 Guest UCI 初始化成功。复现使用 `scripts/macos-android-distribution-smoke.sh`，保留 `result.json`、`uwb-lifecycle.tsv`、UCI 字节计数、`uwb-dumpsys.txt` 和 `installed-android-gates.json`。当前距离报告是确定性 FiRa v2 模拟，不代表物理 RF、角度或 CCC 认证。

### macOS Modem 显示可用但 Android 仍是无 RIL

先检查 `modem-lifecycle.tsv`、`modem-adapter-pids.txt` 和对应 run 的 `modem-adapter-launch-v2.json`。Guest 必须读到 `ro.boot.modem_simulator_ports=9697`，`init.svc.vendor.ril-daemon` 为 `running`，`pidof libcuttlefish-rild` 非空，`service list` 含 Radio HAL，`gsm.operator.numeric` 与 `dumpsys telephony.registry` 均出现测试运营商 `00101`。Host 侧 `/tmp/binder_rpc_vsock_<guest_cid>_9697.sock` 必须是 mode `0600` 且 adapter 进程唯一。

不要手工 `setprop`、重启 ril-daemon 或把 `OUT_OF_SERVICE` 软件占位当作通过。禁用 Modem 才应保持 no-RIL；启用时由 typed 启动参数和正式 component 完成。复现使用安装包 runner，并保留首次/第二次 Radio 证据。当前 AT baseline 不代表通话、短信、IMS、5G、数据附着或真实运营商认证。

### macOS NFC 控件可用但 Android NFC 消失

先在该 run 证据中检查 `nfc-lifecycle.tsv`，并通过包内 ADB 回读 `getprop init.svc.nfc_hal_service`、`service check nfc` 与 Cuttlefish NFC HAL 进程；同时确认同一 run 的 Casimir component 已发布 ready、进程仍存活且 Guest NCI 输出文件大小大于零。启用 NFC 的期望状态是 HAL `running`、binder `found`，Type 2/Type 4/移除动作后仍保持该状态，第二次启动也必须重复成立。

运行时设备策略负责恢复 package、启动 `nfc_hal_service` 和执行 framework enable；不要用手工 `start`、重启 Guest 或被动 framework 模拟掩盖生命周期错误。只有实例明确禁用 NFC 时才应移除 package 并停止 HAL。复现使用 `scripts/macos-android-distribution-smoke.sh`，保留 `result.json`、`nfc-lifecycle.tsv`、动作响应和 `installed-android-gates.json`。

### 快速切换实例后界面跳回或操作落到旧实例

HD 的侧栏与页面 WebView 共用 Host 状态，原生标题栏消费同一平台中立快照；每次轮询都携带唯一快照代次。选择实例、创建、保存设置、导入 Payload 或实例操作完成会使更早的请求失效；陈旧或重复回包只触发新一轮同步，不能写回 UI。侧栏电源菜单还把目标实例 UUID 随操作提交，目标详情未完成同步时会保持按钮禁用，宿主也会拒绝不匹配目标。

默认启动 HD 时，Windows 与 macOS 都会在制品扫描、Host 连接和 WebView 创建前查找同一默认 profile；若已运行，只恢复并激活原窗口后退出第二进程。需要并行验证时必须显式传入不同的 `--data-root`，该模式不会争用默认单实例合同，也不会被默认窗口激活逻辑误选。

复现前先运行 `scripts/macos-ui-concurrency-smoke.sh --output <absolute-dir>` 并保留 `macos-ui-interaction.json` 与 `web-ui-concurrency-gate.json`。若仍出现跳回，收集 `logs/ui-v2.jsonl.*`、操作前后的实例 UUID 和快照时间；不要通过降低轮询频率或取消目标校验掩盖竞态。

### Microdroid 多轮运行后变慢或资源持续增长

对待发布的 `HD.app` 运行 `scripts/macos-microdroid-soak-smoke.sh --app <HD.app> --output <fresh-absolute-dir> --cycles 20`。development 候选必须显式增加 `--development-package`；release 候选禁止使用开发绕过。门禁会输出逐轮 `cycles.jsonl`、汇总 `result.json` 和 `microdroid-soak-gate.json`。

先比较第 1 轮热身值和最后一轮：文件描述符或线程单调增长通常意味着 Host/Worker 清理遗漏；data root 增长超过预算时检查实例删除后的 run、upload、descriptor 与日志保留；Ready 时间上升时对照各轮 `start.stderr` 和 run journal。不要以重启 Host 清空指标来通过门禁，脚本要求所有轮次复用同一 Host 和 data root，最后才执行 `shutdown --stop-all`。

### Android 点击启动后长时间没有进入 VM 启动

先运行 `scripts/macos-artifact-hash-performance-smoke.sh --app <HD.app> --output <fresh-absolute-dir> --budget-seconds 20`。它使用运行时真实 no-follow/完整 SHA-256 路径读取包内 16.5 GB rootfs，并输出 `result.json` 与 `artifact-hash-performance-gate.json`。摘要不一致按供应链篡改处理，不能用缓存或跳过校验规避；摘要正确但超预算时，确认构建包含 macOS CommonCrypto 平台适配，且未回退到 portable `sha2`。该门禁只定位严格哈希阶段，不替代 installed Android 的 Ready、ADB、网络和帧门禁。

### Host 无法启动或第二个 Host 被拒绝

同一 data root 只能有一个 Host。先用 `hdctl --no-start-host health` 检查；不要删除存活 Host 的 `host.lock`。若 descriptor 存在但进程身份无效，客户端会拒绝并拉起新 Host，Host 自身在持有锁后原子更新 descriptor。

### Worker/VM 异常退出

Worker exit monitor 会写 run result 并清理受管进程树；Host 的周期刷新会消费 Failed 或丢失的精确 Worker identity，并按上述有界策略恢复。回读 instance 的 `worker` identity、Worker descriptor、`child_pid`、`cleanup_pending` 和 lease audit。只有精确 PID+启动标记匹配时才允许终止，防止 PID 重用误杀。

Windows 可重复故障注入入口：

```powershell
.\scripts\windows-fault-injection.ps1 -InstanceId <UUID> -Output out\windows-fault-injection
```

runner 依次终止 frame producer、crosvm、Worker 和 Host，验证新 run/frame generation、替代 Worker、Host 同 run 重连、严格 Ready/零拷贝以及最终 Stopped 清理。开发迭代默认继承 `HD_DEV_FAST_ARTIFACTS=1`，证据明确标记未重复执行 capability/bundle 校验。

若 Worker descriptor 丢失但 `worker.lock` 仍被持有，Host 会通过确定性 endpoint 和实例 secret 认证 Worker 并重建 descriptor；不要手工删除 lock 或租约。endpoint 无法认证时实例保持 `Recovering`，先保留现场并生成诊断。

### Windows ABI/缺 DLL

目标必须为 `x86_64-pc-windows-gnu`，十四个 HD exe、原生 UI 图形依赖、crosvm/gfxstream、Microdroid 工具和 Android SDK 工具必须来自同一发布链。运行 `build.bat` 或根 `build_all.bat`；不要用 MSVC DLL 补齐。正式目录使用 GNU `xtask` 从明确的输入生成，输出必须不存在：

```powershell
cargo run --target x86_64-pc-windows-gnu -p xtask -- package `
  --target-dir <gnu-release-dir> `
  --runtime-dir <crosvm-gfxstream-microdroid-runtime-dir> `
  --adb <platform-tools-dir>\adb.exe `
  --aapt2 <build-tools-dir>\aapt2.exe `
  --output <fresh-package-dir>
```

打包器拒绝缺失、符号链接或非普通文件输入，并复制 `AdbWinApi.dll`、`AdbWinUsbApi.dll` 与 platform-tools NOTICE。完成前会清空环境并只保留 Windows 系统目录 PATH，依次执行包内 `crosvm --help`、`vm --help`、`adb version`、`aapt2 version`；任何开发机 PATH 偶然补齐的依赖都会使门禁失败。保留该输出及 `objdump -p` PE audit，Android 候选还要用包内 ADB 对当前回环端口执行 `get-state` 与 `getprop sys.boot_completed`。

## 日志与诊断

运行数据：

```text
runs/<instance>/<run>/manifest.json
runs/<instance>/<run>/events.jsonl[.1...]
runs/<instance>/<run>/result.json
runs/<instance>/<run>/crosvm.stdout.log
runs/<instance>/<run>/crosvm.stderr.log
logs/host-v2.jsonl.*
logs/ui-v2.jsonl.*
logs/worker-<instance>-v2.jsonl.*
logs/lease-audit-v2.jsonl
```

生成诊断：

```powershell
hdctl diagnostics --instance-id <UUID>
hdctl diagnostics --instance-id <UUID> --include-guest-logs
```

Guest logs 只在 Ready 且 ADB 可用时采集。诊断包为受限大小的 `.tar.zst`，包含 manifest 和逐文件 SHA-256；API 另返回最终 archive SHA-256，并对 secret/token/authorization 等字段做大小写不敏感脱敏。报告问题时提供诊断包、archive hash、instance/run/operation UUID、HD/crosvm/gfxstream commit、Rust/MinGW 版本和对应 gate report，不只提供 UI 截图。
