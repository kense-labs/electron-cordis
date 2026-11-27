# `electron-cordis` 开发计划（M0–M5）

默认桌面发行版的产品目标是在同版本、同平台和同架构下直接替换官方 Electron，并在同一候选产物上实现内部模块插件化、减小安装体积和提升性能。既有应用沿用原生 API、ABI、进程与安全默认值。Cordis 服务可由应用显式接入，Electron Lite 由 `--electron-lite` 单独构建和验收。 [目标架构](./electron-cordis-architecture.md#目标整体架构)

## 当前基线

| 项目 | 现状 | 待验证 |
| --- | --- | --- |
| 源码与工具链 | Chromium 位于 `src/`，Electron fork 位于 `src/electron/`。本机 macOS arm64 使用完整 Xcode 27.2，已分别从源码生成 Cordis 与同一上游提交的原版 Testing `Electron.app` 和 `dist.zip`。Release Framework 已链接，符号生成已接入单线程内存限制。Linux x64 已用 `DEPS` 锁定的工具链生成桌面构建图，从源码链接并运行 `chrome_sandbox`，并核对了 Ninja 列出的 63,985 个桌面源码输入。 | 完成 macOS Release 符号和发行包，以及 Linux x64 桌面源码候选。 |
| 兼容性 | 上游提交为 `26919e55ea`。两份 Testing ZIP 解包后均已通过同一组桌面 API、Renderer、IPC 和子进程探针，并加载和调用了同一份 ABI `154` 原生扩展。 | 逐项比较两份 Testing 产物运行上游测试套件的通过、失败与跳过结果。 |
| 桌面开发 | macOS arm64 源码版、Linux arm64 官方基线和 Linux x64 官方基线均已用真实窗口验证 `just dev` 启动、参数透传、正常关闭、终端 `Ctrl+C` 和启动器被强制终止后的进程与 Socket 清理。Linux x64 在离线 QEMU 来宾中验证了临时目录清理以及渲染进程的独立 PID 命名空间、`NoNewPrivs=1` 和 `Seccomp=2`。Windows x64 的 Supervisor 已在 Wine 中验证退出和进程树回收。 | 对 Linux x64 源码候选重复桌面与沙箱对照。在 Windows x64 原生环境验证桌面启动、IPC 和退出清理。 |
| Cordis | 源码版桌面应用已验证认证先于或晚于 UI 就绪时均能进入 `core_active`，并验证启动意图投递、按需插件识别和会话回收。Linux x64 转译容器已通过签名插件的离线安装、升级、守护进程重启后回滚和篡改拒绝测试。 | 用实际应用验证权限、按需能力调用和 UI 重启后的状态恢复。 |
| 发布与性能 | Testing 包可用于开发和功能验证。Release 的体积、启动时间和内存对照等待正式包。 | 在相同机器上测量 Cordis 与同版本官方 Release 包。使用 Inkstone 所需的 Electron 43.2.0 候选包完成应用验收。 |

合并后的源码和构建产物统一在一个 gclient 工作区。Electron Git 操作在 `src/electron/` 执行，开发命令在工作区根目录执行，构建产物写入 `src/out/`。 [构建机与环境要求](./build-host-requirements.md) · [Electron 构建与源码布局](https://www.electronjs.org/docs/latest/development/build-instructions-gn)

## M0 获取首个桌面源码产物

**优先级 P0 · 先在本机 macOS arm64 执行。**

1. 复用已同步的 Chromium 工作区，记录 fork 提交与待验证的本地变更、上游提交、依赖修订、SDK 和 GN 参数。变更上游依赖后再同步 gclient 并重新生成配置。
2. 在工作区根目录用 `just build-electron` 生成 `inkstone-desktop` 默认配置并在 `src/out/Testing` 编译 `electron`，保存编译日志。用独立输出目录准备同一个上游提交和同等构建配置的原版对照，保留现有工作树改动。官方 nightly 下载包用于开发冒烟测试。
3. 从工作区根目录显式指定本地源码产物，记录可执行文件路径和构建来源，并从运行中的 Electron 采集 `process.versions`。

   ```sh
   ELECTRON_CORDIS_BINARY="$PWD/src/out/Testing/Electron.app/Contents/MacOS/Electron" just dev
   ```

**验收**　两个源码构建均能独立启动同一个普通 Electron 应用。`just dev` 使用本仓产物启动示例窗口，显示正确的 Profile，并通过真实 RPC 到达 `ui_ready`。

## M1 验证原版 Electron 兼容性

**优先级 P0 · 依赖 M0 的两份产物。**

- 用 `just verify-desktop <上游可执行文件> <候选可执行文件> [原生插件.node]` 直接启动两份指定产物，比较同基线版本与 ABI、主进程 CJS/ESM 导出和属性描述符、同步 NativeImage、Session 协议隔离、Renderer 默认隔离及跨进程 IPC。验证 `child_process.fork()` 和 `utilityProcess.fork()` 都能运行同一个 Node 脚本、完成消息往返，并报告相同的 Electron 版本和原生模块 ABI。
- 用 `just verify-desktop-specs <上游可执行文件> <候选可执行文件> <报告目录>` 复用 `npm run test` 的原生夹具构建，在同一份上游 `spec/` 上分别运行两个产物。按 Vitest 项目、文件和用例比较通过、失败及跳过状态。单侧运行中断、出现未处理异常或缺失报告时，保留另一侧结果并标记本次验收不确定。可追加测试文件名、`--project parallel` 或 `--test-name-pattern '<正则>'` 筛选专项。报告会记录实际选择条件，运行日志定期显示当前项目、文件和用例。macOS 焦点用例需要已解锁的图形会话。
- 遇到系统调用阻塞 Electron 主线程时，可给同源对照追加 `--worker-idle-timeout <毫秒>`。由 Vitest 父进程监测工作进程的 IPC 活动并终止超时的工作进程，其他测试继续运行。报告记录该阈值，工作进程异常退出会使本次对照无法用于验收。
- 对照 `process.versions` 的 Electron、Chromium、Node 和 `modules` 字段。用同基线上游头文件编译现有 `object-wrap` 等原生测试扩展，把**同一份 `.node` 文件**交给原版和候选运行时加载并调用。有真实应用时，复用其该版本的原生依赖做额外验证。 [Electron 测试指南](https://www.electronjs.org/docs/latest/development/testing) · [Electron 原生 Node 模块](https://www.electronjs.org/docs/latest/tutorial/using-native-node-modules)
- 加入 Cordis 专项回归。默认桌面 Profile 和兼容层保留已有 Electron 导出、同步调用语义和属性描述符。普通应用直接启动时，验证它可以独立运行；显式接入 Cordis 的应用再经 Supervisor 验证同一组 Electron 行为。

**验收**　输出包含执行数、跳过数和差异的双运行时报告。指定关键用例全通过，原生扩展可加载执行；上游回归相对同基线原版无新增失败或跳过。后续各平台以各自的原版对照结果作为发布依据。

本机两份同源 Testing 产物的并行批次各运行了 92 个规格文件，记录 4,881 个用例。候选侧有三项加密用例超时，原版侧有一项 DevTools 用例超时而候选侧通过。加密专项按原版优先、候选优先两种顺序复跑，结果一致。实际 Keychain 探针还确认了异步 Safe Storage 和磁盘加密 Cookie 在原版与候选版之间双向读回。完整兼容性验收仍需覆盖剩余规格及批次中的暂时性差异。

## M2 固化一键桌面开发流程

**优先级 P1 · 可与 M0 并行完善脚本，源码版验收依赖 M0。**

- 保留根目录 `just dev` 和 `just dev ./my-app`，支持应用目录、主进程入口和参数透传。开发日志输出选择的 Electron 路径和版本、Profile 与启动阶段。日常开发优先使用本地构建，缺少产物时使用锁定的官方 Electron 缓存。CI 和发布验证显式指定候选路径，路径无效时直接报错。
- 清理脚本对旧 `electron-cordis-checkout` 目录名的隐式依赖，支持 `ELECTRON_CORDIS_CHECKOUT` 显式指定其他工作区。保留 `just dev-headless`，用同一套 Profile 生成器校验桌面与无界面配置。
- 覆盖首次启动、缓存启动、自定义入口及带空格路径、应用启动失败、关闭最后一个窗口、终端 `Ctrl+C`、启动器被强制终止和重新启动。确认应用主进程能连接 Supervisor 管理的会话，退出后进程和 IPC 端点可回收。macOS arm64 通过后验证 Linux x64 的私有 Unix Socket，并完成 Windows x64 的 Named Pipe 和进程退出验证。

**验收**　每个平台以实际通过的用例标记支持状态。macOS 上分别用官方缓存和源码版运行完整开发流程，默认示例与自定义应用都能稳定启动和清理。

## M3 验证 Cordis 生命周期与权限隔离

**优先级 P1 · 复用 M2 的桌面入口和独立无界面入口。**

- 覆盖 `pre_ui` → `ui_ready` → 应用认证就绪。验证迟到、重复或失效的 readiness 和认证事件，及认证前后能力装载的差异。用 `cargo test --locked --workspace` 维护 Rust 基线，并执行桌面端到端测试。
- 验证 UI 崩溃和重启期间 Supervisor 持有原来的单写卷锁，daemon 状态可恢复使用。测试 IPC 权限、Renderer 的 preload 白名单、上下文隔离和沙箱，确认未经授权的请求和旧内嵌 KRun LoadPlan 会被拒绝。
- 外部 MCP、ACP、Host Native Contract 和 LLM Gateway 按需配置。外部宿主负责其隔离环境，内置宿主进程仅用于受信任的本地执行。能力迁移和 JS 兼容层分别验证错误语义、取消和资源清理。

**验收**　普通 Electron 应用和 Cordis 应用可在各自入口稳定运行。桌面和无界面测试覆盖授权成功、拒绝、UI 重启、会话清理及卷锁，Renderer 只接触获准暴露的 API。 [架构设计](./electron-cordis-architecture.md)

## M4 发布候选与实际应用验证

**优先级 P2 · 依赖目标平台的 M1–M3 验收。**

- 先产出 macOS arm64 候选包，随后按需要扩展 Windows x64、Linux x64 和其他架构。在目标平台用 `just verify-desktop --archives <原版.zip> <候选.zip> [原生插件.node]` 依次解包并运行同一套兼容性探针。每次运行后清理该包的临时副本和解包目录，再处理下一份 ZIP。报告记录实际测试的 ZIP 校验值。复用上游构建与测试工作流，归档提交、依赖修订、构建参数、工具链版本、测试报告和包校验值。预构建缓存按这些构建输入和目标平台区分。按平台验证打包、签名、离线安装与首次调用、升级、卸载和回滚。
- 在 macOS 用 `just benchmark-desktop <原版.zip> <候选.zip>` 从两个同版本 ZIP 解包，交替启动 20 轮。每轮使用独立进程和用户目录，沿用系统文件缓存。报告记录 ZIP 的 SHA-256 和字节数、首帧就绪 P50/P95、进程树的 RSS、内存足迹和 CPU 时间。Release 验收会核对这里测量的 ZIP 与兼容性报告中的 ZIP 校验值。
- 可访问端砚应用后，经 `electronDist` 注入对应版本的候选包，与正式包采用独立应用 ID、数据目录和更新通道。验收命令以应用仓库为准，覆盖登录隔离、PDF / Office 预览、拖拽、`better-sqlite3`、更新与回滚。两套安装包做同机灰度比较，记录启动时间、内存和包体积。
- 若端砚继续依赖 `electron@43.2.0`，用该版本源码、依赖和 ABI `148` 单独构建并重复兼容矩阵。若端砚升级到当前基线，按新基线重建原生依赖并重新跑应用回归。端砚当前是有条件的发布目标，首个同基线 Electron Cordis 产物可独立验收。 [Electron 43.2.0 ABI 配置](https://raw.githubusercontent.com/electron/electron/v43.2.0/build/args/all.gn)
- 端砚当前从应用资源目录读取 Science Pack 和飞书 CLI，已校验的 macOS arm64 开发版 Science Pack 有 5,048 个文件，占 231,097,553 字节。实际候选包继续包含应用所需资源。Slim 模式可通过配置预检查看排除项。交付按需版前，需要验证签名二进制资源的安装与回滚，并让端砚在首次使用时从已验证的安装目录解析资源。

**验收**　同一默认桌面候选通过兼容性、安装升级及性能验收。对照组使用同应用提交、Electron 版本、打包配置、签名状态和目标设备。记录安装器及安装态大小、冷启动首次交互 P50/P95、首次功能就绪、峰值内存和 CPU。产物、测量和兼容报告可追溯。实际应用完成同机对照和回滚演练后，再进入该应用的灰度发布。

ZIP 对照入口已用 macOS 同源原版和 Cordis Testing 归档验证。Linux x64 在限 1 核、768 MiB 的离线来宾中使用两份含同一官方 Electron 的归档验证了工具流程，包括解包、原生扩展加载、渲染进程隔离、子进程 IPC 和临时目录清理。Windows x64 在 Wine 中验证了 `just verify-desktop` 的参数传递、官方 43.2.0 ZIP 解包和临时目录清理。官方 Electron 的桌面探针在该环境未完成。Linux 源码候选与 Windows 原生对照仍待验证。

本机的官方 macOS arm64 nightly 样本解包后共 262.53 MiB，统计口径为实际存放在包内的普通文件逻辑大小。其中 Electron Framework 占 211.67 MiB（80.6%），SwiftShader 占 16.01 MiB，`resources.pak` 占 12.64 MiB，ICU 数据占 10.19 MiB。默认发行版以此类构成为测量基线，优先核实框架链接产物和可合并的重复资源。延迟初始化的收益记入启动和内存，释放磁盘空间的收益单独实测。对照组沿用上游 Release 的 PGO 配置。

macOS arm64 的 `just install-desktop` 已用 Cordis Testing 发行 ZIP 验证。APFS 实际占用从 576 MiB 降到 216 MiB。257 个普通文件的 SHA-256 和权限逐项相同，14 个符号链接的指向一致。桌面 API 探针、原生扩展和 `just dev` 启动均通过。官方 nightly 的签名副本在 HFS+ ULFO DMG 上保留此类压缩，复制到 APFS 后占用从 262 MiB 降至 133 MiB，DMG 下载体积增加 5.5%。正式 Release 的体积和冷启动测量仍采用同版本对照。 [安装命令](./build-host-requirements.md)

同源 macOS arm64 Testing ZIP 的 274 个文件与链接条目路径一致。原版为 199,348,230 字节，Cordis 为 199,328,162 字节，相差约 20 KiB。Electron Framework 可执行文件占这些 ZIP 约 84%。Framework 的 `__TEXT` 段大小一致，文件大小差额主要来自 `__LINKEDIT` 中编译对象路径的长度。下载体积优化以去除调试符号后的 Release 产物为测量基线。

对同一份 Cordis Testing 产物，Deflate 6 和 Deflate 9 生成的 ZIP 中，584 个条目的内容、权限和时间戳一致。各测一轮，Deflate 9 节省 680,810 字节（0.34%），打包时间由 15.24 秒升到 30.55 秒。默认桌面 ZIP 继续使用 Deflate 6。

## M5 按需构建与扩展能力

默认桌面兼容性通过后，再按 Profile 评估可选能力。以同基线的默认桌面包记录实测收益和功能差异。

| 方向 | 交付与验收 |
| --- | --- |
| Electron Lite | 在独立产物中验证 GN 条件注册和裁剪。按需启用 PDF、打印、拼写检查等能力，为被裁剪的模块列出差异并覆盖目标应用回归。 |
| Rust 桌面能力 | 分批迁移安全存储、剪贴板、对话框、系统交互、托盘、通知、录屏与更新，并在启用的 Profile 验证 API、权限和错误行为。 |
| 外部 Harness | 在独立测试环境中验证 `Cordis-over-Wire`、`@dsh/cordis-bridge`、Shadow Fiber 和外部 MCP / ACP 调用，覆盖插件装载、取消、断连和清理。 |
| 零 Chromium Headless | Linux x64 已构建 1.94 MiB 的独立归档，解包后验证了清单、离线启动、零 UI 进程和有序退出。GNU/Linux 验收在 Ubuntu 22.04 的 x86-64 转译容器中执行。后续继续覆盖 CI 使用和其他平台。 |
| 体积与交付 | 测量 `bundled` / `on-demand`、大型套件按需安装、Node / PDFium 去重的收益。Cordis 复用同一启用记录安装文本插件与已签名二进制归档。端砚仍需把首次调用接到已校验的安装目录，并验证正式签名和正式包回滚。 |

二进制插件由宿主下载，`pluginInstaller.installArchive(manifest, archivePath)` 接受本地 gzip tar 的绝对路径。发布者使用 Ed25519 签名以下紧凑 UTF-8 JSON 数组，`archive_sha256` 以小写十六进制记录。`entries` 计入普通文件和目录，`unpacked_file_bytes` 为普通文件逻辑大小之和。目标平台和架构采用 Rust 的 `OS` 与 `ARCH` 名称，例如 `macos` 与 `aarch64`。

```sh
just pack-plugin-archive path/to/prepared-science-pack src/out/plugin-archives/science-pack \
  --plugin-id @dsh/plugin-science-pack --version 1.0.4 \
  --target-os macos --target-arch aarch64 --signing-key path/to/ed25519-private.pem
```

命令将 `archive.tar.gz` 与 `manifest.json` 一次性写入输出目录。同一批文件和签名密钥可复现相同产物，容许重复运行。私钥存放在插件目录外，安装端仅需对应的可信公钥。

```text
["plugin-archive-v1", plugin_id, version, os, arch, archive_bytes, archive_sha256, unpacked_file_bytes, entries]
```

`manifest` 携带上述数组中除首元素外的字段及 `ed25519_signature_hex`。守护进程使用已配置的可信公钥验签，再验证独立归档快照的 SHA-256 后解包。同卷 macOS APFS 优先使用写时复制克隆，跨卷或克隆不可用时使用分块复制。实测安装 Science Pack 1.0.4 时，约 80 MiB 的归档无需占用第二份物理空间。归档支持普通文件和目录，保留 Unix 读取及执行位。验证通过后才更新启用记录。应用将安装目录当作只读资源，运行数据和缓存写入应用数据目录。启动 Science Pack 的 Python 时传入 `-B`，以免在安装目录生成字节码缓存。

桌面宿主可为每个按需插件创建一个 `createOnDemandPluginResolver({ installer, pluginId, acquireArchive, acceptsVersion })`。把返回函数接到首次需要资源的调用点。解析器每次读取当前启用记录，直接使用兼容的已安装版本，包含兼容的回滚版本。插件缺失或当前版本超出应用接受范围时，解析器获取并安装符合要求的签名归档。并发调用共享同一次请求，下载或安装失败后可重试。端砚的私有 Python Cell 可在调用 `loadPrivatePythonEnvironment` 前解析目录。

Science MCP 保留约 2 MiB 的目录和桥接源码供端砚校验。已用端砚现有的 `BundledScienceRuntimeProvider` 证明，安装解释器之前可读取 23 个服务及 232 个工具描述，安装后返回相同目录。首次实际调用可通过签名归档安装 Science Pack，重启后从插件目录启动同一个 Python 桥接器。只有 `catalog.json` 的目录会触发完整性校验错误。端砚 Slim 预览按此保留 `science-mcp`，按需交付 `science-pack`。

飞书 CLI 1.0.93 已以签名归档验证。测试分别用官方 Electron 43.2.0 和本仓源码版启动桌面，由端砚现有的 `createLarkCli` 从插件目录启动二进制文件。两次并发请求共享一次安装，重启后复用缓存。正式集成需在收到有效的飞书操作时解析可执行文件，并保留端砚原有的权限检查和账户目录隔离。

发布源有固定地址时，宿主可用 `createRemotePluginResolver({ installer, pluginId, manifestUrl, archiveUrl, acceptsVersion })`。首次调用先获取清单，由守护进程使用已配置的可信公钥验签，再从宿主配置的 HTTPS 地址分块下载归档。下载与安装各自检查大小和 SHA-256，临时归档在完成或出错后清理。`signal` 可取消下载，`timeoutMs` 默认 15 分钟。

首批交付是 **macOS arm64 源码构建产物、与上游同提交的兼容对照报告、以及指向该产物的 `just dev` 启动记录**。首次编译时长以本机实测为准，其他平台按构建机可用性逐个推进。
