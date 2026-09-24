/// 应用 manifest：tauri-build 的默认清单 + `longPathAware`。
///
/// Windows 10 1607+ 支持超过 260 字符的路径，但**必须由进程 manifest 显式声明**
/// 才生效（否则 Win32 API 仍在 MAX_PATH 处截断）。本程序面向"下载/转码/合并"
/// 场景，用户输出目录与素材目录都可能很深，路径被静默截断会表现为"文件不存在"
/// 这类无法复现的故障。
///
/// 内容 = tauri-build 2.6.3 自带的 windows-app-manifest.xml 原文（Common-Controls
/// 依赖，保证控件主题一致）+ 一段 windowsSettings。DPI 感知由 tao/wry 在启动时
/// 通过 API 设置，执行级别默认 asInvoker，都无需在清单里重复。
const APP_MANIFEST: &str = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() {
    // 图标经 tauri-build 编译为 Windows 资源（PE 资源节）嵌入 exe，但 tauri-build
    // 只对 tauri.conf.json / bundle.resources / 前端产物声明 rerun-if-changed，
    // **不包含图标**。而 cargo 的规则是：build script 只要声明了任意一个
    // rerun-if-changed，就不再按"整个包目录 mtime"做兜底监听 —— 于是替换图标后
    // build script 不会重跑，Windows 资源也不会重新生成。
    //
    // 在本地通常察觉不到（首次构建或 cargo clean 会重建），但在复用 target/ 的 CI
    // （Swatinem/rust-cache）上，旧的资源与产物被缓存恢复后会被原样复用，
    // 表现为 exe 图标永远停留在旧版本。这里显式声明，保证换图标必然触发重建。
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.png");
    // 清单内联在本文件里：改它就是改构建产物，必须触发重跑
    println!("cargo:rerun-if-changed=build.rs");

    tauri_build::try_build(
        tauri_build::Attributes::new().windows_attributes(
            tauri_build::WindowsAttributes::new().app_manifest(APP_MANIFEST),
        ),
    )
    .expect("tauri-build 失败");
}
