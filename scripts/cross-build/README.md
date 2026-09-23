# Linux 交叉编译 Windows 单 EXE（cargo-xwin + zig，已实测）

在 Linux 上用 `cargo-xwin` + `zig` 的 MSVC 兼容 wrapper 交叉编译
`x86_64-pc-windows-msvc`，产出**单 EXE，不依赖 WebView2Loader.dll**。

> CI（`.github/workflows/release.yml`）使用 Windows runner 原生编译，本通道是本地备用
> / 无 Windows 机器时的交付通道。两者产物同为 MSVC 单 EXE。

## 前置依赖

| 组件 | 版本要求 | 安装 |
|------|---------|------|
| Rust | stable（含 `x86_64-pc-windows-msvc` target） | `rustup target add x86_64-pc-windows-msvc` |
| cargo-xwin | 0.23+ | `cargo install cargo-xwin` |
| zig | 0.13+（实测 0.16） | 从 zig 官网下载解压到任意位置，或用发行版包管理器安装 |
| python3 + pefile | 任意 | `pip install pefile`（仅验证用） |

xwin 首次构建会自动下载 MSVC SDK 并缓存（`~/.cache/xwin` 或 `XWIN_CACHE_DIR`），
不需要 sudo，不需要 Windows 机器。

## 工具链 Wrapper（本目录 4 个脚本）

Linux 上没有 MSVC 链接器，xwin 只提供 SDK 头/库，链接与资源编译由 zig 完成。
本目录的 4 个脚本是 zig 的 MSVC 兼容 wrapper，**需复制/软链到 `$PATH` 下**。
zig 可执行文件需可通过 `zig` 命令找到：要么把 zig（或软链）放进同一目录，
要么用环境变量 `ZIG=/path/to/zig` 指定：

| 脚本 | 作用 | 内部命令 |
|------|------|----------|
| `clang-cl` | MSVC 风格 C/C++ 编译器 | `zig cc --driver-mode=cl`（过滤 `--`） |
| `lld-link` | MSVC 链接器 | `zig lld-link`（过滤 `-flavor link`） |
| `llvm-rc` | 资源编译器（.rc → .res） | `zig rc`（过滤 `/no-preprocess`） |
| `x86_64-w64-mingw32-windres` | GNU 资源编译器（备用） | `zig rc` |

## 构建命令（workspace 根执行）

```bash
export PATH="$HOME/.cargo/bin:<wrapper 所在目录>:$PATH"
export ZIG=<zig 可执行文件绝对路径>   # 若 zig 不在 PATH 中
cargo xwin build --release --target x86_64-pc-windows-msvc -p ytdlp-gui
```

产物：`target/x86_64-pc-windows-msvc/release/ytdlp-FFmpeg-GUI.exe`

> 注意：workspace 根（`-p ytdlp-gui` 指定包）产物在根 `target/`，不是 `src-tauri/target/`。

## 验证单 EXE（导入表）

```bash
python3 -c "
import pefile
pe = pefile.PE('target/x86_64-pc-windows-msvc/release/ytdlp-FFmpeg-GUI.exe')
dlls = {e.dll.decode().lower() for e in pe.DIRECTORY_ENTRY_IMPORT}
assert 'webview2loader.dll' not in dlls, dlls
print('导入 DLL 数:', len(dlls)); print('OK: 无 WebView2Loader.dll（单 EXE 依赖成立）')
"
```

导入表应只含系统 DLL（kernel32/user32/shell32/ole32 等）与 VCRUNTIME140，
**无 WebView2Loader.dll**。

## 为什么 MSVC 目标不需要 DLL

webview2-com crate 在 MSVC 目标下使用 `raw-dylib` 延迟加载 WebView2Loader，
运行时由系统 WebView2 Runtime 解析，不静态导入 DLL。GNU 目标（mingw）则会静态
导入，必须配套 DLL——所以本项目统一走 MSVC 目标。

## 实测已知情况（可放心忽略）

- 每次构建出现 `clang-cl: Compiler family detection failed ...` warning：
  cc crate 对 clang-cl 的编译器族探测失败，**不阻塞构建**，可忽略。
- `cargo clippy` 交叉检查请用 `cargo xwin clippy -p ytdlp-gui --target x86_64-pc-windows-msvc`；
  直接 `cargo clippy --target` 会因找不到 MSVC 链接器而报 "GNU compiler is not supported"。
- 增量构建很快（工具链/SDK 已缓存后，改 Rust 代码约 1-2 分钟）。

## 相关

- CI 流程：`.github/workflows/release.yml`（Windows runner 原生编译 + zip + gh-release）。
- 本地 Windows 打包脚本：`scripts/build.ps1`。
