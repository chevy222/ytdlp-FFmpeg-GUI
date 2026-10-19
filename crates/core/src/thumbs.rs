//! 封面缩略图：URL 解析结果下载 / 本地文件与下载产物抽帧。
//! 统一落在 `<exe 同级>\config\cache\thumbs\<id>.jpg`，前端经 asset 协议展示。

use crate::exec::{run_capture_deadline, Tool, ToolResolver};
use std::path::{Path, PathBuf};

/// 缩略图相关 ffmpeg 命令的墙钟上限（与本地探测同源：损坏容器/网络盘上的文件
/// 会让"看着很快"的抽帧命令永久挂住，而调用点在解析/下载收尾链路上）。
const THUMB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// 注：这里曾有两个现已删除的函数：
// - `save_remote_thumb` / `fetch_to`：curl 版「远程缩略图 URL → 本地」下载
//   （含 socks5→socks5h 归一化）。解析链路改用 yt-dlp 取封面后失去全部调用方。
// - `collect_written_thumbnail`：从输出目录收集 yt-dlp `--write-thumbnail` 写出的
//   封面。下载命令已不再写 `--write-thumbnail`（见 download.rs build_args），
//   且全仓无任何调用方，属死代码，已删除以免误导。
// 要恢复请连超时/大小上限与代理归一化一起恢复。

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
    // 带截止时间：坏文件/网络盘上 ffmpeg 可能长期不出结果，而本函数在解析/下载
    // 收尾链路上同步执行（与 probe 的 PROBE_TIMEOUT 同口径）。
    match run_capture_deadline(cmd, THUMB_TIMEOUT) {
        Ok(_) if dest.is_file() => Ok(true),
        Ok(_) => Ok(false),
        Err(e) => {
            // 取内嵌封面失败不算错误（可能本就没有封面流）：交给调用方回退抽帧，
            // 但把原因写进日志，别让"回退"变成无法解释的行为
            on_log(format!("内嵌封面提取失败（回退抽帧）：{e}"));
            Ok(false)
        }
    }
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

/// 用 ffmpeg 从视频文件抽取一帧做封面（等比缩放 ≤360 宽）。
/// `on_log`：实际执行的 ffmpeg 命令行回传（条目日志展示用）。
///
/// 首选 `-ss 0.5`（避开片头黑帧）；时长 < 0.5s 的源在该处取不到帧，回退 `-ss 0`
/// 再试一次，避免极短视频/图片型源直接报"未生成输出文件"。
pub fn extract_thumb(
    resolver: &ToolResolver,
    src: &Path,
    dest: &Path,
    on_log: &mut dyn FnMut(String),
) -> Result<(), String> {
    ensure_parent(dest).map_err(|e| e.to_string())?;
    let mut last_err: Option<String> = None;
    for ss in ["0.5", "0"] {
        let args: Vec<String> = ["-y", "-ss", ss, "-i"]
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
        // stderr 由 run_capture_deadline 统一 pipe + 排空；非零退出/超时的报错信息
        // 会带在错误里（不再是"抽帧失败："后面什么都没有）。
        match run_capture_deadline(cmd, THUMB_TIMEOUT) {
            Ok(_) if dest.is_file() => return Ok(()),
            Ok(_) => last_err = Some("抽帧失败：未生成输出文件".into()),
            Err(e) => last_err = Some(format!("抽帧失败：{e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| "抽帧失败".into()))
}

fn ensure_parent(dest: &Path) -> std::io::Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

/// thumb 目录 + 条目文件路径。
///
/// id 经 [`crate::paths::safe_component`] 净化，避免被篡改/损坏的条目 id
/// （含 `..`/分隔符）拼出越界路径导致读写/删除逃出 `thumbs/`。
pub fn thumb_path(cache_dir: &Path, id: &str) -> PathBuf {
    cache_dir
        .join("thumbs")
        .join(format!("{}.jpg", crate::paths::safe_component(id)))
}

/// 删除某条目的缩略图缓存（删除条目/清空列表时调用）。
/// 不清理的话 `config/cache/thumbs/` 会随使用时长无限累积。
pub fn remove_thumb(cache_dir: &Path, id: &str) {
    let _ = std::fs::remove_file(thumb_path(cache_dir, id));
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
