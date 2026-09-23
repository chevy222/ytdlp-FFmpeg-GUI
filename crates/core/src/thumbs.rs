//! 封面缩略图：URL 解析结果下载 / 本地文件与下载产物抽帧。
//! 统一落在 `<exe 同级>\config\cache\thumbs\<id>.jpg`，前端经 asset 协议展示。

use crate::exec::{ChildGuard, Tool, ToolResolver};
use std::path::{Path, PathBuf};

/// 从输出目录收集 yt-dlp `--write-thumbnail` 写出的封面文件。
///
/// yt-dlp 下载时已经拉过封面（用于 --embed-thumbnail），加 --write-thumbnail
/// 后会把它同时写到输出目录（与视频同名，扩展名 .webp/.jpg/.jpeg/.png）。
/// 下载完成后直接取这个文件做列表缩略图，比重新从网络拉或 ffmpeg 抽帧都快。
///
/// 返回找到的封面路径；调用方负责复制到 thumbs 目录并删除原文件。
pub fn collect_written_thumbnail(output_path: &Path) -> Option<PathBuf> {
    let parent = output_path.parent()?;
    let stem = output_path.file_stem()?;
    for ext in ["webp", "jpg", "jpeg", "png"] {
        let candidate = parent.join(format!("{}.{}", stem.to_string_lossy(), ext));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 从远程缩略图 URL 下载到 dest（如 yt-dlp 的 thumbnail）。
///
/// 先直连尝试；失败且调用方给了代理时带 `--proxy` 重试一次——
/// 需要代理的站点（YouTube 等）缩略图服务器直连拉不动，而 yt-dlp 主下载
/// 走的是设置里的代理，缩略图不能因此缺席。
pub fn save_remote_thumb(url: &str, dest: &Path, proxy: Option<&str>) -> Result<(), String> {
    ensure_parent(dest).map_err(|e| e.to_string())?;
    match fetch_to(url, dest, None) {
        Ok(()) => Ok(()),
        Err(e) => match proxy {
            Some(p) if !p.is_empty() => fetch_to(url, dest, Some(p)),
            _ => Err(e),
        },
    }
}

fn fetch_to(url: &str, dest: &Path, proxy: Option<&str>) -> Result<(), String> {
    let tmp = dest.with_extension("tmp.jpg");
    let tmp_str = tmp.to_string_lossy().into_owned();
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-L", "--fail", "-sS", "-o", &tmp_str, url]);
    if let Some(p) = proxy {
        cmd.args(["--proxy", p]);
    }
    // GUI 程序启动控制台子进程会弹出一个黑窗（一闪而过）；这里与其它调用点
    // 保持一致，显式隐藏控制台。
    crate::exec::hide_console(&mut cmd);
    let output = cmd.output().map_err(|e| format!("无法调用 curl：{e}"))?;
    if !output.status.success() || !tmp.is_file() {
        let _ = std::fs::remove_file(&tmp);
        return Err("缩略图下载失败".into());
    }
    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let _ = std::fs::remove_file(&tmp);
        return Err("缩略图为空".into());
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// 抽取文件**内嵌封面**（attached_pic，元数据）——与桌面/播放器显示的封面同源，
/// 不重新截帧。无封面流时返回 `Ok(false)`，由调用方回退抽帧。
pub fn extract_cover(
    resolver: &ToolResolver,
    src: &Path,
    dest: &Path,
    cover_stream_index: Option<usize>,
    on_log: &mut dyn FnMut(String),
) -> Result<bool, String> {
    let Some(idx) = cover_stream_index else {
        return Ok(false);
    };
    ensure_parent(dest).map_err(|e| e.to_string())?;
    let args: Vec<String> = [
        "-y".to_string(),
        "-i".to_string(),
        src.to_string_lossy().into_owned(),
        "-map".to_string(),
        format!("0:{idx}"),
        "-frames:v".to_string(),
        "1".to_string(),
        "-q:v".to_string(),
        "2".to_string(),
        dest.to_string_lossy().into_owned(),
    ]
    .to_vec();
    on_log(crate::exec::display_command("ffmpeg", &args));
    let mut cmd = resolver.command(Tool::Ffmpeg).map_err(|e| e.to_string())?;
    cmd.args(&args);
    let child = ChildGuard::spawn(&mut cmd).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok(out.status.success() && dest.is_file())
}

/// 缩略图统一入口：**优先内嵌封面**（元数据），没有封面流才抽帧。
///
/// 直接抽帧会拿到转码后文件 0.5s 的一帧，与源文件封面/桌面缩略图不一致——
/// 本地文件与转码/合并产物都走这里，保证列表缩略图与文件自带的封面同源。
pub fn ensure_thumb(
    resolver: &ToolResolver,
    src: &Path,
    dest: &Path,
    cover_stream_index: Option<usize>,
    on_log: &mut dyn FnMut(String),
) -> Result<(), String> {
    if extract_cover(resolver, src, dest, cover_stream_index, on_log)? {
        return Ok(());
    }
    extract_thumb(resolver, src, dest, on_log)
}

/// 用 ffmpeg 从视频文件抽取一帧做封面（-ss 0.5 首帧附近，等比缩放 ≤360 宽）。
/// `on_log`：实际执行的 ffmpeg 命令行回传（条目日志展示用）。
pub fn extract_thumb(
    resolver: &ToolResolver,
    src: &Path,
    dest: &Path,
    on_log: &mut dyn FnMut(String),
) -> Result<(), String> {
    ensure_parent(dest).map_err(|e| e.to_string())?;
    let args: Vec<String> = ["-y", "-ss", "0.5", "-i"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(src.to_string_lossy().into_owned()))
        .chain(
            ["-frames:v", "1", "-vf", "scale=360:-2", "-q:v", "3"]
                .iter()
                .map(|s| s.to_string()),
        )
        .chain(std::iter::once(dest.to_string_lossy().into_owned()))
        .collect();
    on_log(crate::exec::display_command("ffmpeg", &args));
    let mut cmd = resolver.command(Tool::Ffmpeg).map_err(|e| e.to_string())?;
    cmd.args(&args);
    let child = ChildGuard::spawn(&mut cmd).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() || !dest.is_file() {
        return Err(format!(
            "抽帧失败：{}",
            crate::exec::decode_text(&out.stderr)
        ));
    }
    Ok(())
}

fn ensure_parent(dest: &Path) -> std::io::Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

/// thumb 目录 + 条目文件路径。
pub fn thumb_path(cache_dir: &Path, id: &str) -> PathBuf {
    cache_dir.join("thumbs").join(format!("{id}.jpg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_path_layout() {
        let p = thumb_path(Path::new("/x/config/cache"), "abc");
        assert_eq!(p, Path::new("/x/config/cache/thumbs/abc.jpg"));
    }

    #[test]
    fn extract_thumb_bad_source() {
        let resolver = ToolResolver::default();
        let err = extract_thumb(
            &resolver,
            Path::new("no-such-file.mp4"),
            Path::new("/tmp/no-thumb.jpg"),
            &mut |_| {},
        );
        assert!(err.is_err(), "源不存在应报错");
    }

    #[test]
    fn display_command_quotes_spaces() {
        // 含空格/引号的参数必须加引号，否则复制出去的命令不可直接执行
        assert_eq!(
            crate::exec::display_command("yt-dlp", &["-J".into(), "a b.mp4".into()]),
            "yt-dlp -J \"a b.mp4\""
        );
        assert_eq!(
            crate::exec::display_command(
                "ffmpeg",
                &[
                    "-i".into(),
                    "in 1.mp4".into(),
                    "-y".into(),
                    "out.mp4".into()
                ]
            ),
            "ffmpeg -i \"in 1.mp4\" -y out.mp4"
        );
    }
}
