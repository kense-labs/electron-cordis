# `electron-cordis` 构建机与环境要求（分级持久化规范）

> 对应架构与计划：[electron-cordis-architecture.md](./electron-cordis-architecture.md) · [development-plan.md](./development-plan.md)  
> 自动化预检脚本：`python3 electron-cordis/script/check-build-host.py --tier <headless|canary-pack|chromium-base>`

为避免日常迭代被 Chromium 重编阻塞，`electron-cordis` 的构建分为三个独立层级，各层对宿主机硬件、磁盘与工具链的要求严格区分：

Tier 1 可从独立检出的 Electron Cordis 仓库执行。Tier 2 的端砚候选包是可选的应用集成构建，可用 `INKSTONE_WEB_ROOT` 指定端砚 `web/` 的位置。

---

## 一、三级构建环境矩阵

| 维度 | Tier 1：微内核 & 零 Chromium Headless<br>（`headless` · 日常开发/CI） | Tier 2：双轨并行桌面候选包<br>（`canary-pack` · 复用 Prebuilt Base） | Tier 3：从源码编译裁切版 Base<br>（`chromium-base` · 专用构建机） |
| --- | --- | --- | --- |
| **产物** | `dist/cordis-headless/`（~7–15 MiB，`daemon` + `supervisor` + `load-plan.json` + JS 桥接） | `dist/cordis-macos/` 或 `dist/cordis-win/`（`appId=cn.internai.inkstone.cordis`，`cordis-canary` 通道） | `Prebuilt Electron-Cordis Base`（按 `cordis.args.gn` 裁除 C 类与已下沉 A 类绑定后的 Electron-Lite 二进制） |
| **CPU / 内存** | 任意 4 核+ / 8 GB+ RAM | 任意 4 核+ / 16 GB+ RAM | **≥ 8 核（推荐 10–16 核+） / ≥ 32 GB RAM**（最低 16 GB，链接期易 OOM） |
| **可用磁盘空间** | **≥ 5 GiB** | **≥ 15 GiB** | **≥ 200 GiB**（`gclient sync` 源码 + `out/Release` 编译产物占 120–180 GiB，macOS 另需 Xcode ~20 GiB；可挂载高速外接 SSD） |
| **系统工具链** | Rust ≥ 1.88 (`cargo`)、Node.js ≥ 22、Python ≥ 3.10、Git | Tier 1 全部 + `pnpm` + 已有 `ELECTRON_CORDIS_DIST`（或干跑验证 `ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN=1`） | **macOS**：完整 `/Applications/Xcode.app`（`xcode-select` 指向 Xcode，不可仅用 CommandLineTools）<br>**Windows**：Visual Studio 2022（Desktop C++ + Windows 10/11 SDK + Debugging Tools）<br>**Linux**：`clang`/`lld` + Chromium sysroot |
| **Chromium 工具链** | 不需要 | 不需要 | **`depot_tools`**（`gclient` / `gn` / `ninja`）或 **`@electron/build-tools`**（`e`） |
| **单次耗时** | 10–30 秒（增量 ~1 秒） | 1–3 分钟 | 首次同步+全量编 **2–5 小时**；后续仅缓存复用 |
| **原生 Electron 影响** | 零修改 | 零修改（严格独立配置与输出目录） | 零修改（独立产物目录与缓存） |

---

## 二、Tier 3（`Prebuilt Electron-Cordis Base`）构建机详细前置条件

### 1. macOS arm64 / x64 构建机
1. **磁盘**：目标构建卷（内置盘或外接 NVMe SSD，如 `/Volumes/BuildSSD/electron-cordis-src`）空闲空间 **≥ 200 GiB**（APFS 区分大小写或默认 APFS 均可）。
2. **完整 Xcode**：
   - 安装与 Electron 43 / Chromium 对应版本的 `/Applications/Xcode.app`；
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

# 2. 由声明式 Profile 生成 cordis.args.gn 与模块注册表
python3 electron-cordis/script/gen-cordis-build.py \
  electron-cordis/profiles/inkstone-desktop.cordis.json \
  --out-dir /path/to/generated
python3 electron-cordis/script/gen-module-registry.py \
  /path/to/generated/build-plan.json \
  --out-dir /path/to/generated

# 3. 同步 Electron/Chromium 源码并将 cordis.args.gn 合并进 out/Release/args.gn
#    （不向 patches/chromium/ 新增侵入式业务补丁，仅启用 electron-cordis/buildflags/cordis_plugins.gni）
gn gen out/Release
ninja -C out/Release electron
ninja -C out/Release electron:electron_dist_zip

# 4. 将预构建 Base 输出路径注入 Tier 2 并行候选打包（原生 Electron 43.2.0 链路零改动）
ELECTRON_CORDIS_DIST=/path/to/prebuilt-electron-cordis-darwin-arm64 \
  bash electron-cordis/script/build-inkstone-cordis.sh inkstone-desktop
```

---

## 四、本机快速自检命令

```bash
# 检查能否编译 Rust 微内核与零 Chromium Headless 发行包（Tier 1）
python3 electron-cordis/script/check-build-host.py --tier headless

# 检查能否打双轨并行桌面候选包（Tier 2）
python3 electron-cordis/script/check-build-host.py --tier canary-pack

# 检查能否从源码编译 Chromium/Electron Base（Tier 3，可指定外接 SSD 挂载点）
python3 electron-cordis/script/check-build-host.py --tier chromium-base --workspace-dir /Volumes/BuildSSD
```
