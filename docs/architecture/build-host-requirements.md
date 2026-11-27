# `electron-cordis` 构建机与环境要求（分级持久化规范）

> 对应架构与计划：[electron-cordis-architecture.md](./electron-cordis-architecture.md) · [development-plan.md](./development-plan.md)  
> 自动化预检脚本：`python3 electron-cordis/script/check-build-host.py --tier <headless|canary-pack|chromium-base>`

为避免日常迭代被 Chromium 重编阻塞，`electron-cordis` 的构建分为三个独立层级，各层对宿主机硬件、磁盘与工具链的要求严格区分：

Tier 1 可从独立检出的 Electron Cordis 仓库执行。Tier 2 的端砚候选包是可选的应用集成构建，可用 `INKSTONE_WEB_ROOT` 指定端砚 `web/` 的位置。

---

## 一、三级构建环境矩阵

| 维度 | Tier 1：微内核 & 零 Chromium Headless<br>（`headless` · 日常开发/CI） | Tier 2：双轨并行桌面候选包<br>（`canary-pack` · 复用 Prebuilt Base） | Tier 3：从源码编译裁切版 Base<br>（`chromium-base` · 专用构建机） |
| --- | --- | --- | --- |
| **产物** | `dist/cordis-headless/`（`daemon` + `supervisor` + `load-plan.json` + JS 桥接，Linux x64 样例为 4.67 MiB） | `dist/cordis-macos/` 或 `dist/cordis-win/`（`appId=cn.internai.inkstone.cordis`，`cordis-canary` 通道） | `Prebuilt Electron-Cordis Base`（默认使用上游 Electron 构建设置，可显式启用 Electron-Lite 裁剪） |
| **CPU / 内存** | 任意 4 核+ / 8 GB+ RAM | 任意 4 核+ / 16 GB+ RAM | **≥ 8 核（推荐 10–16 核+） / ≥ 32 GB RAM**（最低 16 GB，链接期易 OOM） |
| **可用磁盘空间** | **≥ 5 GiB** | **≥ 15 GiB** | **≥ 200 GiB**（`gclient sync` 源码 + `out/Release` 编译产物占 120–180 GiB，macOS 另需 Xcode ~20 GiB；可挂载高速外接 SSD） |
| **系统工具链** | Rust ≥ 1.88 (`cargo`)、Node.js ≥ 22、Python ≥ 3.10、Git | Tier 1 全部 + `pnpm` + 已有 `ELECTRON_CORDIS_DIST`（或干跑验证 `ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN=1`） | **macOS**：完整 `/Applications/Xcode.app`（`xcode-select` 指向 Xcode，不可仅用 CommandLineTools）<br>**Windows**：Visual Studio 2022（Desktop C++ + Windows 10/11 SDK + Debugging Tools）<br>**Linux**：`clang`/`lld` + Chromium sysroot |
| **Chromium 工具链** | 不需要 | 不需要 | **`depot_tools`**（`gclient` / `gn` / `ninja`）或 **`@electron/build-tools`**（`e`） |
| **单次耗时** | 10–30 秒（增量 ~1 秒） | 1–3 分钟 | 首次同步+全量编 **2–5 小时**；后续仅缓存复用 |
| **原生 Electron 影响** | 零修改 | 零修改（严格独立配置与输出目录） | 零修改（独立产物目录与缓存） |

---

## 二、Tier 3（`Prebuilt Electron-Cordis Base`）构建机详细前置条件

桌面 Profile 默认保留上游 Electron 的功能开关和模块注册表。向 `gen-cordis-build.py` 传入 `--electron-lite` 可启用 Electron Lite 裁剪，应用可显式接入 Cordis 兼容层。

当前 `main` 的上游基线为 Electron 提交 `26919e55ea`，其最近的发布标签是 `v47.0.0-nightly.20261123`，原生模块 ABI 为 `154`。缺少 Git 标签时，构建从 `build/electron-cordis-upstream-version` 读取版本。更新上游基线时应同步更新此文件。若要直接替换依赖 Electron `43.2.0`（ABI `148`）的应用，需要用 `43.2.0` 源码和依赖构建并验证替换产物。

### 1. macOS arm64 / x64 构建机
1. **磁盘**：目标构建卷（内置盘或外接 NVMe SSD，如 `/Volumes/BuildSSD/electron-cordis-src`）空闲空间 **≥ 200 GiB**（APFS 区分大小写或默认 APFS 均可）。
2. **完整 Xcode**：
   - 安装与目标 Electron / Chromium 对应版本的 `/Applications/Xcode.app`；
   - 切换开发者目录并接受许可：
     ```bash
     sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
     sudo xcodebuild -license accept
     xcodebuild -version
     ```
   - 仅安装 `/Library/Developer/CommandLineTools` **无法**编译 Chromium/Electron（缺少 `actool`/`ibtool`/完整 macOS SDK）。
3. **工具链与网络**：
   - 安装 `depot_tools` 并加入 `PATH`（或安装 `@electron/build-tools` 提供 `e` 命令）；
   - 网络可稳定访问 `chromium.googlesource.com`、`github.com/electron/electron` 与 Google Cloud Storage prebuilts（必要时配置 `HTTP_PROXY` / `HTTPS_PROXY` 与 `NO_PROXY`）。

### 2. Windows x64 构建机
1. **磁盘**：NTFS 卷可用空间 **≥ 200 GiB**（路径尽量短，如 `D:\ec-build`）。
2. **编译器与 SDK**：Visual Studio 2022（勾选 `Desktop development with C++`、Windows 10/11 SDK ≥ 10.0.22621 与 **Debugging Tools for Windows**）。
3. **系统设置**：开启 Windows Developer Mode（允许非管理员创建 symlink），安装 `depot_tools` 且置于 `PATH` 最前，设置 `DEPOT_TOOLS_WIN_TOOLCHAIN=0`。

### 3. Linux x64 / arm64 构建机
1. **磁盘**：可用空间 **≥ 200 GiB**，内存 ≥ 32 GB。
2. **依赖**：`depot_tools`、Python 3.10+、`git`、`pkg-config`、Chromium `install-build-deps.sh` 所需基础库（或官方 sysroot）。

---

## 三、Tier 3 标准编译接入流程（复用本仓 Profile 与门禁）

在满足 Tier 3 条件的构建机上执行：

```bash
# 1. 环境预检（确保磁盘 ≥200 GiB、Xcode/SDK、gclient/gn/ninja 全部达标）
python3 electron-cordis/script/check-build-host.py --tier chromium-base --workspace-dir /path/to/build-volume

# 2. 在 gclient 工作区根目录构建 Release、发行 ZIP 和原生模块头文件
just build-electron --release

# 3. 将与端砚锁定版本一致的预构建 Base 注入 Tier 2 并行候选打包
ELECTRON_CORDIS_DIST=/path/to/prebuilt-electron-cordis-darwin-arm64 \
  bash electron-cordis/script/build-inkstone-cordis.sh inkstone-desktop
```

`ELECTRON_CORDIS_DIST` 接受发行 ZIP、解包目录或带标准 ZIP 文件名的缓存目录。相对路径以端砚 `web/` 为基准。候选配置会读取端砚锁定的 Electron 版本，并核对候选包的版本、目标架构和关键运行文件。校验只读取必要的元数据和文件头。

`just inspect-inkstone-resources --web-root <端砚的 web 目录> --platform darwin` 会列出 Slim 配置预览排除的资源、已准备普通文件的逻辑大小和缺失文件。选择 Windows x64 时用 `--platform win32`。可追加 `--distribution <发行 ZIP 或目录>`，同时核对目标平台和端砚锁定的 Electron 版本。预检读取配置和文件元数据。

macOS 候选默认输出 `.app`。设置 `ELECTRON_CORDIS_DMG=1` 后，构建同时输出安装 DMG 和更新 ZIP。`cordis-canary-mac.yml` 和差分索引引用 ZIP。安装 DMG 可在生成后优化体积，启用磁盘镜像签名时保留已签名的字节。端砚的 `direct-mac-candidate` 模式保留应用自身的自动更新限制。

Windows 候选使用端砚识别的 `direct-windows-verification` 模式，并在打包前核对签名更新助手和独立配置中的应用身份。当前端砚的更新助手只接受正式版、Alpha 和 Beta 的 Windows 身份。Windows x64 候选打包需要应用端为 `cn.internai.inkstone.cordis` 提供相应配置。

macOS 上可运行 `just install-desktop src/out/Release/dist.zip ./installed/electron-cordis`，把发行 ZIP 安装到新目录。APFS 支持透明文件系统压缩，应用继续按原路径读取同一份文件内容。安装完成后，命令输出 `Electron.app` 的路径。

---

## 四、本机快速自检命令

### 启动桌面开发模式

在仓库根目录运行 `just dev`，即可构建 Cordis daemon 和 Supervisor，并打开桌面开发窗口。窗口展示同一守护进程的 Profile 和状态。关闭窗口或按 `Ctrl+C` 可结束会话。启动器进程退出并关闭控制管道时，Supervisor 停止应用进程，有序关闭守护进程，并清理本次开发会话的临时目录。Windows 入口通过 PATH 上的 `python` 调用 Python 3。

`just dev ./my-electron-app` 接受应用目录或主进程入口，后续参数传给 Electron。应用主进程可用 `CordisDaemonClient({ socketPath: process.env.ELECTRON_CORDIS_SOCKET })` 连接当前会话，并在 UI 就绪后调用 `notifyUiReady()`。内置窗口提供了这个接法的示例。

Linux x64 的 `inkstone-desktop` daemon 和 Supervisor 已交叉编译为静态链接的 ELF，并在禁用网络、内存上限 64 MiB 的 Linux x64 容器中完成无界面启动和有序退出。守护进程返回了 Profile 健康状态、已加载的内置插件和 4 个按需插件的登记信息。同一守护进程在含中文和空格的路径中验证了签名 ELF 插件的安装、升级、重启后回滚、拒绝损坏签名和归档，以及两个版本的可执行文件启动。验收容器运行在 Apple Silicon 的 x86-64 转译环境中。

Linux x64 的 `just dev` 已在限 1 核、768 MiB 的 Ubuntu 22.04 转译容器中使用锁定的官方 Electron 通过真实窗口、Unix Socket RPC、参数透传、正常关闭、终端 `Ctrl+C` 和强制终止启动器测试。退出后渲染进程、应用、Supervisor、daemon、Socket 和临时目录均被清理。直接运行同一个官方 Electron 时，渲染进程也使用独立的 PID 命名空间，`NoNewPrivs` 为 1，`Seccomp` 为 0。最小 seccomp 程序在转译环境无法安装过滤器，在同机 ARM64 容器和 256 MiB 的 QEMU x86-64 来宾中均成功安装。完整 Linux x64 浏览器沙箱验收需要支持 seccomp 的 x86-64 内核环境。 [Chromium Linux 沙箱](https://chromium.googlesource.com/chromium/src/+/main/sandbox/linux)

同一官方 Electron 和 Cordis 开发启动链已在配置为 1 核、768 MiB 的离线 QEMU x86-64 Ubuntu 来宾中复测。直接启动与 `just dev` 创建的渲染进程均采用独立 PID 命名空间、`NoNewPrivs=1` 和 `Seccomp=2`。`just dev` 的三种退出路径均通过进程、Socket 和临时目录清理验证。

Windows 守护进程的 `--socket` 和其他本地 RPC 入口接受 `\\.\pipe\<名称>`。命名管道只允许当前账户的本地客户端连接，JavaScript 客户端沿用 `socketPath`。Windows `just dev` 为每次启动分配独立管道。Windows Job Object 负责回收同一进程树中仍存活的进程。Windows x64 已通过 MSVC 目标编译检查，并用 LLVM MinGW 交叉链接了用于验收的 daemon 和 Supervisor。两个可执行文件仅导入 Windows 系统 DLL。Wine 上已验证标准输入输出、双客户端命名管道通信和 Shadow 请求响应。Supervisor 在窗口进程正常退出、启动器断开和 Supervisor 被强制终止时均回收了测试进程树。在同一 Wine 环境直接调用 Win32 API，也重现了重复绑定被接受。Wine 使用 ASCII 字符集时，直接调用系统 API 创建含非 ASCII 字符的目录会失败。用 UTF-8 启动同一套测试后，Windows x64 产物在中文目录通过签名安装、升级和回滚。Windows 原生验收还需覆盖完整桌面、键盘中断、重复绑定保护和签名插件安装。

命令优先使用当前工作区 `src/out/` 中可启动的 Electron 构建，检查二进制和必要的 Chromium 资源。`ELECTRON_OUT_DIR` 可指定构建目录，相对路径从 `src/out/` 解析，指定目录的产物缺失或不完整时会直接报错。`ELECTRON_CORDIS_CHECKOUT` 可指定另一个 gclient 工作区根目录，`ELECTRON_CORDIS_BINARY` 可直接指定可执行文件，两者的相对路径均从调用 `just` 的目录解析。自动选择时若缺少本地构建，命令会使用仓库锁定的官方 Electron，首次下载后缓存到 `dist/cordis-dev/`。`just dev-headless` 可单独启动无界面运行时。

### 按 Profile 构建

- `just build-electron` 在 `src/out/Testing` 增量构建默认兼容桌面版。默认使用 2 个 GN 工作线程和最多 2 个本地构建任务，链接任务最多同时运行 1 个，每个 Blink Web IDL 分片在单个 Python 进程中生成。用 `ELECTRON_CORDIS_BUILD_JOBS=1 just build-electron` 将 GN 工作线程和本地构建任务的上限都降到 1。
- 在内存紧张的机器上，可先跑 `just build-electron --bindings-only` 单独完成 Blink 绑定生成，再用 `just build-electron` 复用同一输出目录。中断后也可以复用已完成的构建结果。`--gen-only` 用于只生成 GN 配置。并发限制降低峰值，链接等单个任务的内存仍取决于产物和工具链。
- `just build-electron --dist` 复用 Testing 产物运行上游 `electron:testing_build`，生成 `dist.zip`、`mksnapshot.zip`、`xcache.zip` 和 `gen/node_headers.tar.gz`。`just build-electron --release` 在 `src/out/Release` 运行上游 `electron:release_build`，并生成 `chromedriver.zip` 和平台调试符号。独立发布的 Chromium 版 FFmpeg 位于 `src/out/ffmpeg/ffmpeg.zip`，可用 `just build-electron --ffmpeg-only` 单独构建。Release 默认以单任务构建，并分别限制 ThinLTO 与 macOS `dsymutil` 的线程数为 1。`ELECTRON_CORDIS_BUILD_JOBS` 设置这些上限。经典 DWARF 链接器在单线程下逐个分析和处理对象，及时释放对象的分析数据。
- Siso 构建默认使用 900 MiB 的 Go 软内存目标。可通过 `GOMEMLIMIT` 覆盖，以适配构建机的内存预算。
- macOS 可用 `just build-electron --guarded --release` 或 `just build-electron-lite dsh-studio --guarded --dist` 开启系统内存监测。默认单任务编译，内存可用比例达到 75% 且系统压力正常时才启动，低于 70% 或压力升高时停止本次编译。命令会打印日志位置，可用 `ELECTRON_CORDIS_MEMORY_START_AT` 和 `ELECTRON_CORDIS_MEMORY_RESERVE` 调整阈值。保护触发时退出码为 75，内存恢复后重跑同一命令可继续使用已完成的对象文件。
- macOS 可通过 `ELECTRON_CORDIS_MAC_SDK_PATH` 为当前构建指定 SDK 目录。Electron 43.2.0 的 Chromium 基线使用 macOS 26.5 SDK。在装有 Xcode 27 的机器上，仍可用这一设置让旧版 LLD 读取兼容的 SDK。
- `just build-cordis inkstone-headless` 构建 release daemon 和 Supervisor。Cargo 仅启用 Profile 内以 `bundled` 交付的 Rust 内置插件，并按 feature 组合缓存产物。`just dev`、`just dev-headless` 和 headless 打包共用此选择逻辑。
- Linux 的 Cordis Release 默认移除调试信息和非运行所需的 ELF 符号。需要保留发布版符号时可设置 `CARGO_PROFILE_RELEASE_STRIP=none`。对 Linux x64 桌面 Profile 的静态 ELF 做符号裁剪后，daemon 和 Supervisor 两个文件合计由 8.19 MiB 降至 5.75 MiB。代码段和只读数据保持字节一致，离线启动、退出和签名插件安装升级回滚测试均通过。Linux x64 Headless 用同一套工具链验证，逻辑体积从 14.44 MiB 降到 4.67 MiB，gzip 归档从 4.14 MiB 降到 1.94 MiB，解包后通过离线启动和退出验证。
- `just build-electron-lite dsh-studio --gen-only` 生成 `src/out/CordisLite-dsh-studio`。省略 `--gen-only` 后继续编译 Electron。Profile 控制内容追踪、应用内购买、推送通知、Touch Bar、`sharedTexture` 和 `net` 的公共导出及专用原生实现。原生剪贴板和安全存储分别由 `@dsh/electron-legacy-clipboard` 与 `@dsh/electron-legacy-safe-storage` 控制。选择 Rust 实现的应用可通过 `@dsh/electron-compat` 接入 Cordis 服务。`BrowserView`、`ShareMenu`、`netLog` 和 `ServiceWorkerMain` 的顶层导出也受 Profile 控制。应用可以通过 `session.netLog` 记录网络日志，并通过 `session.serviceWorkers` 管理服务工作线程和使用 IPC。
- `BrowserView` 的 Profile 开关同时控制 `BrowserWindow` 上的旧视图管理方法。Lite 应用可以用 `WebContentsView` 添加子视图，并用普通 `MenuItem` 的 `shareMenu` 角色在 macOS 菜单中提供分享。
- 将所选内置项声明为 `bundled` 才会纳入本次构建。`on-demand` 是包交付声明，daemon 和 Electron 编译均跳过此类条目。

### 检查构建环境

```bash
# 检查能否编译 Rust 微内核与零 Chromium Headless 发行包（Tier 1）
python3 electron-cordis/script/check-build-host.py --tier headless

# 检查能否打双轨并行桌面候选包（Tier 2）
python3 electron-cordis/script/check-build-host.py --tier canary-pack

# 检查能否从源码编译 Chromium/Electron Base（Tier 3，可指定外接 SSD 挂载点）
python3 electron-cordis/script/check-build-host.py --tier chromium-base --workspace-dir /Volumes/BuildSSD
```
