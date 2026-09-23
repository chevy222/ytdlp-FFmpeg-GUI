//! CLI 入口参数解析（§UL-09）：`ytdlp-ffmpeg-gui --url <URL> [--cookies <path>]
//! [--dir <path>] [--yt-dlp-path <path>] [--deno-path <path>]`。
//! 裸位置参数按 URL 处理；`--url` 可重复。解析结果供主进程启动与单实例转发共用。

/// 解析后的 CLI 入参。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliArgs {
    pub urls: Vec<String>,
    /// 本次调用输出目录覆盖（不写 config.json）
    pub dir: Option<String>,
    /// 本次调用 cookie 文件覆盖（Netscape 格式）
    pub cookies: Option<String>,
    /// 本次调用 yt-dlp 可执行文件覆盖
    pub yt_dlp_path: Option<String>,
    /// 本次调用 deno 可执行文件覆盖（PO-Token 服务）
    pub deno_path: Option<String>,
}

impl CliArgs {
    /// 是否有任何待执行动作（URL 或路径覆盖）。
    pub fn has_action(&self) -> bool {
        !self.urls.is_empty()
            || self.dir.is_some()
            || self.cookies.is_some()
            || self.yt_dlp_path.is_some()
            || self.deno_path.is_some()
    }
}

/// 取下一个参数作为选项值。
///
/// 缺值、空值、以及"值其实是另一个选项"（以 `-` 开头）都视为**缺值且不消耗**该
/// 参数，让它自己按选项解析。否则 `--cookies --dir D:\out` 会把 `--dir` 当成
/// cookie 文件路径、再把 `D:\out` 当裸参数收进 URL 列表（`add_url` 又按"非 http"
/// 丢弃），整条命令静默无动作，用户完全看不出哪里错了。
fn take_value(it: &mut std::slice::Iter<'_, String>) -> Option<String> {
    let v = it.clone().next()?;
    if v.is_empty() || v.starts_with('-') {
        return None;
    }
    it.next();
    Some(v.clone())
}

/// 解析命令行参数（不含 argv[0]）。
///
/// 支持：`--url <u>`（可多次）/ `--cookies <p>` / `--dir <p>` /
/// `--yt-dlp-path <p>` / `--deno-path <p>`；其余裸参数视为 URL。
pub fn parse_cli_args(args: &[String]) -> CliArgs {
    let mut out = CliArgs::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" | "-u" => {
                if let Some(v) = take_value(&mut it) {
                    out.urls.push(v);
                }
            }
            "--cookies" => out.cookies = take_value(&mut it),
            "--dir" => out.dir = take_value(&mut it),
            "--yt-dlp-path" => out.yt_dlp_path = take_value(&mut it),
            "--deno-path" => out.deno_path = take_value(&mut it),
            "--help" | "-h" | "--version" | "-v" => { /* 参数保留在 urls 外的语义：忽略 */
            }
            other => {
                if !other.is_empty() && !other.starts_with('-') {
                    out.urls.push(other.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_url_flag_repeated() {
        let c = parse_cli_args(&a(&[
            "--url",
            "https://a.com/v1",
            "--url",
            "https://b.com/v2",
        ]));
        assert_eq!(c.urls.len(), 2);
        assert!(c.urls[0].contains("a.com"));
        assert_eq!(c.dir, None);
    }

    #[test]
    fn parse_bare_positional_as_url() {
        let c = parse_cli_args(&a(&["https://a.com/x"]));
        assert_eq!(c.urls, a(&["https://a.com/x"]));
    }

    #[test]
    fn parse_all_overrides() {
        let c = parse_cli_args(&a(&[
            "--url",
            "https://a.com/x",
            "--dir",
            "D:\\out",
            "--cookies",
            "C:\\c.txt",
            "--yt-dlp-path",
            "C:\\tools\\yt-dlp.exe",
            "--deno-path",
            "C:\\tools\\deno.exe",
        ]));
        assert_eq!(c.dir.as_deref(), Some("D:\\out"));
        assert_eq!(c.cookies.as_deref(), Some("C:\\c.txt"));
        assert_eq!(c.yt_dlp_path.as_deref(), Some("C:\\tools\\yt-dlp.exe"));
        assert_eq!(c.deno_path.as_deref(), Some("C:\\tools\\deno.exe"));
        assert!(c.has_action());
    }

    #[test]
    fn parse_missing_value_ignored() {
        let c = parse_cli_args(&a(&["--dir"]));
        assert_eq!(c.dir, None);
        assert!(!c.has_action());
    }

    #[test]
    fn option_value_does_not_swallow_next_flag() {
        // `--cookies` 后面跟的是另一个选项 → 视为缺值，且不能把 --dir 消耗掉
        let c = parse_cli_args(&a(&["--cookies", "--dir", "D:\\out"]));
        assert_eq!(c.cookies, None);
        assert_eq!(c.dir.as_deref(), Some("D:\\out"));
        assert!(c.urls.is_empty(), "D:\\out 不应被当成 URL：{:?}", c.urls);
    }

    #[test]
    fn has_action_covers_pure_overrides() {
        // 只带覆盖项（不带 URL）也是"有动作"：主进程要把 --dir 应用到本实例
        assert!(parse_cli_args(&a(&["--dir", "D:\\out"])).has_action());
        assert!(parse_cli_args(&a(&["--deno-path", "C:\\deno.exe"])).has_action());
        assert!(!parse_cli_args(&a(&["--help"])).has_action());
    }

    #[test]
    fn parse_empty_input() {
        let c = parse_cli_args(&[]);
        assert!(c.urls.is_empty());
        assert!(!c.has_action());
    }
}
