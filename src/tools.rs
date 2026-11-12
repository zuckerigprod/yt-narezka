// Внешние программы (yt-dlp, ffmpeg, deno) скачиваются при первом запуске
// в папку данных приложения. Пользователю ничего ставить не нужно.

use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
const EXE: &str = ".exe";
#[cfg(not(windows))]
const EXE: &str = "";

#[derive(Clone)]
pub struct Tools {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    pub fn title(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Deno => "deno",
        }
    }
}

/// Что качать: url архива/файла и какие файлы из него достать.
struct Source {
    url: String,
    // None - url указывает прямо на исполняемый файл
    from_zip: Option<&'static [&'static str]>,
    save_as: &'static str,
}

impl Tools {
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("bin") }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}{EXE}"))
    }

    pub fn ytdlp(&self) -> PathBuf {
        self.path("yt-dlp")
    }
    pub fn ffmpeg(&self) -> PathBuf {
        self.path("ffmpeg")
    }
    pub fn deno(&self) -> PathBuf {
        self.path("deno")
    }

    pub fn is_installed(&self, tool: Tool) -> bool {
        match tool {
            Tool::YtDlp => self.ytdlp().exists(),
            Tool::Ffmpeg => self.ffmpeg().exists() && self.path("ffprobe").exists(),
            Tool::Deno => self.deno().exists(),
        }
    }

    pub fn missing(&self) -> Vec<Tool> {
        [Tool::YtDlp, Tool::Ffmpeg, Tool::Deno]
            .into_iter()
            .filter(|t| !self.is_installed(*t))
            .collect()
    }

    /// Скачивает инструмент. progress получает долю 0..1.
    pub fn install(&self, tool: Tool, progress: &mut dyn FnMut(f32)) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let sources = sources(tool)?;
        let n = sources.len() as f32;
        for (i, src) in sources.iter().enumerate() {
            let mut sub = |p: f32| progress((i as f32 + p) / n);
            let tmp = self.dir.join(format!("{}.download", src.save_as));
            download(&src.url, &tmp, &mut sub)
                .with_context(|| format!("не удалось скачать {}", src.url))?;
            match src.from_zip {
                None => {
                    let dst = self.path(src.save_as);
                    let _ = fs::remove_file(&dst);
                    fs::rename(&tmp, &dst)?;
                    make_executable(&dst)?;
                }
                Some(names) => {
                    let res = self.unzip(&tmp, names);
                    let _ = fs::remove_file(&tmp);
                    res?;
                }
            }
        }
        progress(1.0);
        Ok(())
    }

    fn unzip(&self, archive: &Path, names: &[&str]) -> Result<()> {
        let file = fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(file).context("архив повреждён")?;
        let mut found = 0;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            let entry_name = entry.name()?.replace('\\', "/");
            let file_name = entry_name.rsplit('/').next().unwrap_or("").to_string();
            if !names.contains(&file_name.as_str()) {
                continue;
            }
            let dst = self.dir.join(&file_name);
            let mut out = fs::File::create(&dst)?;
            std::io::copy(&mut entry, &mut out)?;
            drop(out);
            make_executable(&dst)?;
            found += 1;
        }
        if found < names.len() {
            bail!("в архиве нет нужных файлов: {}", names.join(", "));
        }
        Ok(())
    }

    /// Обновляет yt-dlp - YouTube часто ломает старые версии.
    pub fn update_ytdlp(&self) -> Result<String> {
        let out = command(&self.ytdlp()).arg("-U").output()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        Ok(text.trim().to_string())
    }
}

fn sources(tool: Tool) -> Result<Vec<Source>> {
    let gh = "https://github.com";
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let list = match (tool, os) {
        (Tool::YtDlp, "windows") => vec![Source {
            url: format!("{gh}/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"),
            from_zip: None,
            save_as: "yt-dlp",
        }],
        (Tool::YtDlp, "macos") => vec![Source {
            url: format!("{gh}/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos"),
            from_zip: None,
            save_as: "yt-dlp",
        }],
        (Tool::YtDlp, "linux") => vec![Source {
            url: format!("{gh}/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux"),
            from_zip: None,
            save_as: "yt-dlp",
        }],
        (Tool::Ffmpeg, "windows") => vec![Source {
            url: format!(
                "{gh}/yt-dlp/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip"
            ),
            from_zip: Some(&["ffmpeg.exe", "ffprobe.exe"]),
            save_as: "ffmpeg",
        }],
        (Tool::Ffmpeg, "macos") => {
            let a = if arch == "aarch64" { "arm64" } else { "amd64" };
            let base = format!("https://ffmpeg.martin-riedl.de/redirect/latest/macos/{a}/release");
            vec![
                Source {
                    url: format!("{base}/ffmpeg.zip"),
                    from_zip: Some(&["ffmpeg"]),
                    save_as: "ffmpeg",
                },
                Source {
                    url: format!("{base}/ffprobe.zip"),
                    from_zip: Some(&["ffprobe"]),
                    save_as: "ffprobe",
                },
            ]
        }
        (Tool::Deno, "windows") => vec![deno_source("x86_64-pc-windows-msvc", "deno.exe")],
        (Tool::Deno, "macos") => {
            let t = if arch == "aarch64" { "aarch64-apple-darwin" } else { "x86_64-apple-darwin" };
            vec![deno_source(t, "deno")]
        }
        (Tool::Deno, "linux") => vec![deno_source("x86_64-unknown-linux-gnu", "deno")],
        _ => return Err(anyhow!("{} для этой системы не поддерживается", tool.title())),
    };
    Ok(list)
}

fn deno_source(target: &str, file: &'static str) -> Source {
    let names: &'static [&'static str] = if file == "deno.exe" { &["deno.exe"] } else { &["deno"] };
    Source {
        url: format!("https://github.com/denoland/deno/releases/latest/download/deno-{target}.zip"),
        from_zip: Some(names),
        save_as: "deno",
    }
}

fn download(url: &str, dst: &Path, progress: &mut dyn FnMut(f32)) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("ytdown-narezki")
        .timeout(None)
        .build()?;
    let mut resp = client.get(url).send()?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    let mut out = fs::File::create(dst)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        if total > 0 {
            progress(done as f32 / total as f32);
        }
    }
    out.flush()?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<()> {
    Ok(())
}

/// Command без всплывающего окна консоли на Windows.
pub fn command(program: &Path) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c.env("PYTHONIOENCODING", "utf-8");
    c.env("PYTHONUTF8", "1");
    c
}
