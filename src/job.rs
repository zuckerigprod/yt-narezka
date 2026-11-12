// Фоновая работа: получение информации о видео, скачивание и нарезка.

use crate::tools::{Tools, command};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub url: String,
    pub title: String,
    pub duration: Option<f64>,
    pub chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Quality {
    Best,
    P1080,
    P720,
    P480,
    AudioMp3,
}

impl Quality {
    pub const ALL: [Quality; 5] =
        [Quality::Best, Quality::P1080, Quality::P720, Quality::P480, Quality::AudioMp3];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Best => "Максимальное",
            Quality::P1080 => "До 1080p",
            Quality::P720 => "До 720p",
            Quality::P480 => "До 480p",
            Quality::AudioMp3 => "Только звук (mp3)",
        }
    }

    fn is_audio(self) -> bool {
        self == Quality::AudioMp3
    }
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct JobParams {
    pub url: String,
    pub info: Option<VideoInfo>,
    pub segments: Vec<Segment>,
    pub out_dir: PathBuf,
    pub temp_root: PathBuf,
    pub quality: Quality,
    pub precise: bool,
    pub subfolder: bool,
    pub keep_full: bool,
}

pub enum Msg {
    Status(String),
    Progress(f32),
    Log(String),
    Info(VideoInfo),
    Done { files: Vec<PathBuf>, dir: PathBuf },
    Failed(String),
    ToolsReady,
}

#[derive(Clone)]
pub struct Ctx {
    pub tx: Sender<Msg>,
    pub cancel: Arc<AtomicBool>,
    pub repaint: eframe::egui::Context,
}

impl Ctx {
    pub fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.repaint.request_repaint();
    }
    fn status(&self, s: impl Into<String>) {
        self.send(Msg::Status(s.into()));
    }
    fn progress(&self, p: f32) {
        self.send(Msg::Progress(p.clamp(0.0, 1.0)));
    }
    fn log(&self, s: impl Into<String>) {
        self.send(Msg::Log(s.into()));
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub fn new_channel() -> (Sender<Msg>, Receiver<Msg>) {
    channel()
}

fn ytdlp_base(tools: &Tools) -> Command {
    let mut c = command(&tools.ytdlp());
    c.args(["--no-playlist", "--encoding", "utf-8", "--color", "never"]);
    c.arg("--ffmpeg-location").arg(&tools.dir);
    if tools.deno().exists() {
        c.arg("--js-runtimes").arg(format!("deno:{}", tools.deno().display()));
    }
    c
}

pub fn fetch_info(tools: &Tools, url: &str, ctx: &Ctx) -> Result<VideoInfo> {
    ctx.status("Получаю информацию о видео...");
    let mut cmd = ytdlp_base(tools);
    cmd.args(["-J", "--no-warnings", "--", url]);
    let mut json = String::new();
    run(cmd, ctx, |line| {
        json.push_str(line);
        json.push('\n');
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&json).context("yt-dlp вернул непонятный ответ")?;
    let title = v["title"].as_str().unwrap_or("video").to_string();
    let duration = v["duration"].as_f64();
    let chapters = v["chapters"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some(Chapter {
                        start: c["start_time"].as_f64()?,
                        end: c["end_time"].as_f64()?,
                        title: c["title"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(VideoInfo { url: url.to_string(), title, duration, chapters })
}

pub fn run_job(tools: Tools, p: JobParams, ctx: Ctx) {
    let temp = p.temp_root.join(format!(
        "job-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let res = job(&tools, &p, &ctx, &temp);
    let _ = fs::remove_dir_all(&temp);
    match res {
        Ok((files, dir)) => ctx.send(Msg::Done { files, dir }),
        Err(e) if ctx.cancelled() => {
            let _ = e;
            ctx.send(Msg::Failed("Отменено".into()))
        }
        Err(e) => ctx.send(Msg::Failed(format!("{e:#}"))),
    }
}

fn job(tools: &Tools, p: &JobParams, ctx: &Ctx, temp: &Path) -> Result<(Vec<PathBuf>, PathBuf)> {
    if p.segments.is_empty() && !p.keep_full {
        bail!("Не задано ни одного отрезка");
    }
    let info = match &p.info {
        Some(i) if i.url == p.url => i.clone(),
        _ => {
            let i = fetch_info(tools, &p.url, ctx)?;
            ctx.send(Msg::Info(i.clone()));
            i
        }
    };
    if let Some(d) = info.duration {
        for (i, s) in p.segments.iter().enumerate() {
            if s.start >= d {
                bail!("Отрезок {}: начало за пределами видео", i + 1);
            }
        }
    }

    fs::create_dir_all(temp)?;
    let source = download(tools, p, ctx, temp)?;

    let title = sanitize(&info.title);
    let out_dir = if p.subfolder { p.out_dir.join(&title) } else { p.out_dir.clone() };
    fs::create_dir_all(&out_dir).context("не удалось создать папку для сохранения")?;

    let ext = if p.quality.is_audio() { "mp3" } else { "mp4" };
    let mut files = Vec::new();
    let total = p.segments.len();
    let width = if total >= 100 { 3 } else { 2 };
    for (i, seg) in p.segments.iter().enumerate() {
        if ctx.cancelled() {
            bail!("Отменено");
        }
        let num = format!("{:0width$}", i + 1);
        let name = sanitize(&seg.name);
        let base = if name.is_empty() {
            format!("{title} - {num}")
        } else {
            format!("{title} - {num} - {name}")
        };
        let dst = unique_path(&out_dir, &base, ext);
        ctx.status(format!("Нарезка {}/{}: {}", i + 1, total, file_name(&dst)));
        ctx.progress(0.0);
        cut(tools, &source, &dst, seg, p, ctx)
            .with_context(|| format!("ошибка при нарезке отрезка {}", i + 1))?;
        files.push(dst);
    }

    if p.keep_full {
        let src_ext = source.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let dst = unique_path(&out_dir, &title, src_ext);
        move_file(&source, &dst)?;
        files.push(dst);
    }
    Ok((files, out_dir))
}

fn download(tools: &Tools, p: &JobParams, ctx: &Ctx, temp: &Path) -> Result<PathBuf> {
    ctx.status("Скачиваю видео...");
    ctx.progress(0.0);
    let mut cmd = ytdlp_base(tools);
    cmd.args(["--newline", "--no-part", "--no-mtime"]);
    cmd.args([
        "--progress-template",
        "download:PROGRESS %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s",
    ]);
    let sort = match p.quality {
        Quality::Best => Some("res,fps,vcodec:h264,acodec:aac"),
        Quality::P1080 => Some("res:1080,fps,vcodec:h264,acodec:aac"),
        Quality::P720 => Some("res:720,fps,vcodec:h264,acodec:aac"),
        Quality::P480 => Some("res:480,fps,vcodec:h264,acodec:aac"),
        Quality::AudioMp3 => None,
    };
    match sort {
        Some(s) => {
            cmd.args(["-f", "bv*+ba/b", "-S", s, "--merge-output-format", "mp4"]);
        }
        None => {
            cmd.args(["-f", "ba/b"]);
        }
    }
    cmd.arg("-o").arg(temp.join("source.%(ext)s"));
    cmd.args(["--", &p.url]);

    let mut stream = 1;
    let mut last = 0.0f32;
    run(cmd, ctx, |line| {
        if let Some(rest) = line.strip_prefix("PROGRESS ") {
            let f: Vec<Option<f64>> = rest.split_whitespace().map(|x| x.parse().ok()).collect();
            let done = f.first().copied().flatten().unwrap_or(0.0);
            let total = f.get(1).copied().flatten().or(f.get(2).copied().flatten()).unwrap_or(0.0);
            if total > 0.0 {
                let frac = (done / total) as f32;
                if frac + 0.2 < last {
                    stream += 1;
                }
                last = frac;
                ctx.progress(frac);
                let speed = f.get(3).copied().flatten().map(human_speed).unwrap_or_default();
                let eta = f
                    .get(4)
                    .copied()
                    .flatten()
                    .map(|e| format!(", осталось {}", crate::timecode::format(e.round())))
                    .unwrap_or_default();
                let what = if p.quality.is_audio() {
                    "звук".to_string()
                } else if stream == 1 {
                    "видео".to_string()
                } else {
                    "звук".to_string()
                };
                ctx.status(format!(
                    "Скачиваю {what}: {:.0}% {speed}{eta}",
                    frac * 100.0
                ));
            }
        } else if line.contains("[Merger]") {
            ctx.status("Склеиваю видео и звук...");
            ctx.log(line);
        } else if !line.trim().is_empty() {
            ctx.log(line);
        }
    })?;

    // берём самый большой готовый файл во временной папке
    let mut best: Option<(u64, PathBuf)> = None;
    for e in fs::read_dir(temp)? {
        let e = e?;
        let path = e.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // промежуточные потоки выглядят как source.f137.mp4 - их пропускаем
        if !name.starts_with("source.") || name.matches('.').count() != 1 {
            continue;
        }
        let len = e.metadata()?.len();
        if best.as_ref().is_none_or(|(l, _)| len > *l) {
            best = Some((len, path));
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| anyhow!("yt-dlp не создал файл"))
}

fn cut(tools: &Tools, src: &Path, dst: &Path, seg: &Segment, p: &JobParams, ctx: &Ctx) -> Result<()> {
    let dur = (seg.end - seg.start).max(0.001);
    let mut cmd = command(&tools.ffmpeg());
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]);
    cmd.args(["-ss", &format!("{:.3}", seg.start)]);
    cmd.arg("-i").arg(src);
    cmd.args(["-t", &format!("{dur:.3}")]);
    if p.quality.is_audio() {
        cmd.args(["-vn", "-c:a", "libmp3lame", "-q:a", "2"]);
    } else if p.precise {
        cmd.args([
            "-map", "0:v:0?", "-map", "0:a:0?",
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "192k",
            "-movflags", "+faststart",
        ]);
    } else {
        cmd.args([
            "-map", "0:v:0?", "-map", "0:a:0?",
            "-c", "copy", "-avoid_negative_ts", "make_zero",
            "-movflags", "+faststart",
        ]);
    }
    cmd.args(["-progress", "pipe:1", "-nostats"]);
    cmd.arg(dst);
    let res = run(cmd, ctx, |line| {
        if let Some(v) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<f64>() {
                ctx.progress((us / 1_000_000.0 / dur) as f32);
            }
        }
    });
    if res.is_err() {
        let _ = fs::remove_file(dst);
    }
    res
}

/// Запускает процесс, построчно отдаёт stdout, следит за отменой.
fn run(mut cmd: Command, ctx: &Ctx, mut on_line: impl FnMut(&str)) -> Result<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("не удалось запустить внешнюю программу")?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let err_tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let tail = err_tail.clone();
    let log_ctx = ctx.clone();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        for line in String::from_utf8_lossy(&buf).lines() {
            if line.trim().is_empty() {
                continue;
            }
            log_ctx.log(line.to_string());
            let mut t = tail.lock().unwrap();
            t.push_back(line.to_string());
            if t.len() > 5 {
                t.pop_front();
            }
        }
    });

    let (tx, rx) = channel::<String>();
    let out_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\r', '\n']);
                    if tx.send(line.to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => on_line(&line),
            Err(RecvTimeoutError::Timeout) => {
                if ctx.cancelled() {
                    let _ = child.kill();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait()?;
    let _ = out_thread.join();
    let _ = err_thread.join();
    if ctx.cancelled() {
        bail!("Отменено");
    }
    if !status.success() {
        let tail: Vec<String> = err_tail.lock().unwrap().iter().cloned().collect();
        let msg = tail
            .iter()
            .rev()
            .find(|l| l.contains("ERROR"))
            .cloned()
            .unwrap_or_else(|| tail.join("\n"));
        bail!("{}", if msg.is_empty() { format!("код завершения {status}") } else { msg });
    }
    Ok(())
}

fn human_speed(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("({:.1} МБ/с)", bps / 1024.0 / 1024.0)
    } else {
        format!("({:.0} КБ/с)", bps / 1024.0)
    }
}

/// Убирает символы, недопустимые в именах файлов на Windows и macOS.
pub fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let mut out = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 120 {
        out = out.chars().take(120).collect();
    }
    let out = out.trim_end_matches(['.', ' ']).to_string();
    // зарезервированные имена Windows
    let upper = out.to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "LPT1", "LPT2"];
    if reserved.contains(&upper.as_str()) { format!("_{out}") } else { out }
}

fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let base = if base.is_empty() { "video" } else { base };
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    p
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if fs::rename(src, dst).is_err() {
        // другой диск - копируем
        fs::copy(src, dst)?;
        fs::remove_file(src)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("a/b: c?"), "a b c");
        assert_eq!(sanitize("Видео  про   котов..."), "Видео про котов");
        assert_eq!(sanitize("CON"), "_CON");
    }
}
