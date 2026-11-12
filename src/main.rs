#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod job;
mod timecode;
mod tools;

use eframe::egui::{self, Color32, RichText};
use job::{Ctx, JobParams, Msg, Quality, Segment, VideoInfo};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use tools::{Tool, Tools};

const APP_NAME: &str = "YT Нарезки";
const UPDATE_EVERY_SECS: u64 = 24 * 3600;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    out_dir: Option<PathBuf>,
    quality: Quality,
    precise: bool,
    subfolder: bool,
    keep_full: bool,
    split_step: String,
    last_update: u64,
}

impl Default for Settings {
    fn default() -> Self {
        let out_dir = directories::UserDirs::new()
            .and_then(|u| u.download_dir().or(u.video_dir()).map(Path::to_path_buf));
        Self {
            out_dir,
            quality: Quality::P1080,
            precise: true,
            subfolder: false,
            keep_full: false,
            split_step: "10".into(),
            last_update: 0,
        }
    }
}

#[derive(Default, Clone)]
struct Row {
    start: String,
    end: String,
    name: String,
}

impl Row {
    fn is_blank(&self) -> bool {
        self.start.trim().is_empty() && self.end.trim().is_empty() && self.name.trim().is_empty()
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Busy {
    No,
    Tools,
    Info,
    Job,
}

struct App {
    tools: Tools,
    data_dir: PathBuf,
    settings_path: PathBuf,
    settings: Settings,

    url: String,
    info: Option<VideoInfo>,
    rows: Vec<Row>,
    bulk_open: bool,
    bulk_text: String,
    split_from: String,
    split_to: String,
    // разбить, как только придёт длительность видео
    pending_split: bool,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    busy: Busy,
    tools_ready: bool,
    status: String,
    progress: Option<f32>,
    error: Option<String>,
    result: Option<(Vec<PathBuf>, PathBuf)>,
    log: Vec<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let dirs = directories::ProjectDirs::from("", "", "YT Narezki");
        let (data_dir, config_dir) = match &dirs {
            Some(d) => (d.data_dir().to_path_buf(), d.config_dir().to_path_buf()),
            None => {
                let base = std::env::temp_dir().join("yt-narezki");
                (base.clone(), base)
            }
        };
        let settings_path = config_dir.join("settings.json");
        let settings: Settings = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let (tx, rx) = job::new_channel();
        let mut app = Self {
            tools: Tools::new(&data_dir),
            data_dir,
            settings_path,
            settings,
            url: String::new(),
            info: None,
            rows: vec![Row::default()],
            bulk_open: false,
            bulk_text: String::new(),
            split_from: String::new(),
            split_to: String::new(),
            pending_split: false,
            tx,
            rx,
            cancel: Arc::default(),
            busy: Busy::No,
            tools_ready: false,
            status: String::new(),
            progress: None,
            error: None,
            result: None,
            log: Vec::new(),
        };
        app.prepare_tools(&cc.egui_ctx);
        app
    }

    fn ctx(&self, egui_ctx: &egui::Context) -> Ctx {
        self.cancel.store(false, Ordering::Relaxed);
        Ctx { tx: self.tx.clone(), cancel: self.cancel.clone(), repaint: egui_ctx.clone() }
    }

    fn save_settings(&self) {
        if let Some(dir) = self.settings_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.settings) {
            let _ = std::fs::write(&self.settings_path, s);
        }
    }

    /// Докачивает недостающие программы и раз в сутки обновляет yt-dlp.
    fn prepare_tools(&mut self, egui_ctx: &egui::Context) {
        let missing = self.tools.missing();
        let need_update = now_secs().saturating_sub(self.settings.last_update) > UPDATE_EVERY_SECS;
        if missing.is_empty() && !need_update {
            self.tools_ready = true;
            return;
        }
        if need_update {
            self.settings.last_update = now_secs();
            self.save_settings();
        }
        self.busy = Busy::Tools;
        self.error = None;
        let tools = self.tools.clone();
        let ctx = self.ctx(egui_ctx);
        let update = need_update && !missing.contains(&Tool::YtDlp);
        std::thread::spawn(move || {
            for t in missing {
                let label = match t {
                    Tool::YtDlp => "Скачиваю yt-dlp (загрузчик видео)",
                    Tool::Ffmpeg => "Скачиваю ffmpeg (нарезка видео)",
                    Tool::Deno => "Скачиваю deno (нужен yt-dlp для YouTube)",
                };
                ctx.send(Msg::Status(format!("Первый запуск. {label}...")));
                let res = tools.install(t, &mut |p| ctx.send(Msg::Progress(p)));
                if let Err(e) = res {
                    // без deno yt-dlp ещё как-то работает, без остального - нет
                    if t == Tool::Deno {
                        ctx.send(Msg::Log(format!("deno не скачался: {e:#}")));
                        continue;
                    }
                    ctx.send(Msg::Failed(format!("Не удалось скачать {}: {e:#}", t.title())));
                    return;
                }
            }
            if update {
                ctx.send(Msg::Status("Проверяю обновления yt-dlp...".into()));
                ctx.send(Msg::Progress(0.0));
                match tools.update_ytdlp() {
                    Ok(out) => ctx.send(Msg::Log(out)),
                    Err(e) => ctx.send(Msg::Log(format!("обновление yt-dlp: {e:#}"))),
                }
            }
            ctx.send(Msg::ToolsReady);
        });
    }

    fn fetch_info(&mut self, egui_ctx: &egui::Context) {
        let url = self.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        self.busy = Busy::Info;
        self.error = None;
        self.result = None;
        self.progress = None;
        let tools = self.tools.clone();
        let ctx = self.ctx(egui_ctx);
        std::thread::spawn(move || match job::fetch_info(&tools, &url, &ctx) {
            Ok(info) => ctx.send(Msg::Info(info)),
            Err(e) => ctx.send(Msg::Failed(format!("{e:#}"))),
        });
    }

    fn segments(&self) -> Result<Vec<Segment>, String> {
        let duration = self.info.as_ref().and_then(|i| i.duration);
        let mut out = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if r.is_blank() {
                continue;
            }
            let n = i + 1;
            let start = timecode::parse(&r.start)
                .ok_or(format!("Отрезок {n}: непонятное начало \"{}\"", r.start))?;
            let end = if r.end.trim().is_empty() {
                duration.ok_or(format!("Отрезок {n}: не указан конец"))?
            } else {
                timecode::parse(&r.end).ok_or(format!("Отрезок {n}: непонятный конец \"{}\"", r.end))?
            };
            if end <= start {
                return Err(format!("Отрезок {n}: конец должен быть позже начала"));
            }
            if let Some(d) = duration {
                if start >= d {
                    return Err(format!(
                        "Отрезок {n}: начало за пределами видео (длина {})",
                        timecode::format(d)
                    ));
                }
            }
            out.push(Segment { start, end, name: r.name.trim().to_string() });
        }
        Ok(out)
    }

    fn start_job(&mut self, egui_ctx: &egui::Context) {
        self.error = None;
        self.result = None;
        let segments = match self.segments() {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        if segments.is_empty() && !self.settings.keep_full {
            self.error = Some("Добавьте хотя бы один отрезок".into());
            return;
        }
        let Some(out_dir) = self.settings.out_dir.clone() else {
            self.error = Some("Выберите папку для сохранения".into());
            return;
        };
        self.save_settings();
        let params = JobParams {
            url: self.url.trim().to_string(),
            info: self.info.clone(),
            segments,
            out_dir,
            temp_root: self.data_dir.join("tmp"),
            quality: self.settings.quality,
            precise: self.settings.precise,
            subfolder: self.settings.subfolder,
            keep_full: self.settings.keep_full,
        };
        self.busy = Busy::Job;
        self.progress = Some(0.0);
        self.status = "Запуск...".into();
        let tools = self.tools.clone();
        let ctx = self.ctx(egui_ctx);
        std::thread::spawn(move || job::run_job(tools, params, ctx));
    }

    fn handle_messages(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Status(s) => self.status = s,
                Msg::Progress(p) => self.progress = Some(p),
                Msg::Log(s) => {
                    self.log.extend(s.lines().map(str::to_string));
                    let extra = self.log.len().saturating_sub(1000);
                    self.log.drain(..extra);
                }
                Msg::Info(info) => {
                    self.info = Some(info);
                    if self.busy == Busy::Info {
                        self.busy = Busy::No;
                        self.status.clear();
                    }
                    if self.pending_split {
                        self.pending_split = false;
                        self.apply_split();
                    }
                }
                Msg::Done { files, dir } => {
                    self.busy = Busy::No;
                    self.progress = None;
                    self.status.clear();
                    self.result = Some((files, dir));
                }
                Msg::Failed(e) => {
                    self.pending_split = false;
                    self.busy = Busy::No;
                    self.progress = None;
                    self.status.clear();
                    self.error = Some(e);
                }
                Msg::ToolsReady => {
                    self.busy = Busy::No;
                    self.tools_ready = true;
                    self.progress = None;
                    self.status.clear();
                }
            }
        }
    }

    fn rows_from_chapters(&mut self) {
        if let Some(info) = &self.info {
            self.rows = info
                .chapters
                .iter()
                .map(|c| Row {
                    start: timecode::format(c.start),
                    end: timecode::format(c.end),
                    name: c.title.clone(),
                })
                .collect();
        }
    }

    /// Кнопка шаблона: без длительности сначала загружаем информацию о видео.
    fn request_split(&mut self, egui_ctx: &egui::Context) {
        self.error = None;
        if self.info.as_ref().and_then(|i| i.duration).is_some() || !self.split_to.trim().is_empty() {
            self.apply_split();
            return;
        }
        if self.url.trim().is_empty() {
            self.error = Some("Сначала вставьте ссылку - нужна длительность видео".into());
            return;
        }
        if !self.tools_ready {
            self.error = Some("Подождите, пока загрузятся компоненты".into());
            return;
        }
        self.pending_split = true;
        self.fetch_info(egui_ctx);
    }

    fn apply_split(&mut self) {
        let parse_opt = |s: &str| -> Result<Option<f64>, ()> {
            if s.trim().is_empty() { Ok(None) } else { timecode::parse(s).map(Some).ok_or(()) }
        };
        let Some(step) = timecode::parse(&self.settings.split_step).filter(|s| *s > 0.0) else {
            self.error = Some("Шаблон: непонятная длина куска".into());
            return;
        };
        let (Ok(from), Ok(to)) = (parse_opt(&self.split_from), parse_opt(&self.split_to)) else {
            self.error = Some("Шаблон: непонятное время в полях \"с\" / \"по\"".into());
            return;
        };
        let duration = self.info.as_ref().and_then(|i| i.duration);
        let from = from.unwrap_or(0.0);
        let Some(mut to) = to.or(duration) else {
            self.error = Some("Не удалось узнать длительность видео - укажите \"по\"".into());
            return;
        };
        if let Some(d) = duration {
            to = to.min(d);
        }
        let parts = timecode::split(from, to, step);
        if parts.is_empty() {
            self.error = Some("Шаблон: \"по\" должно быть позже \"с\"".into());
            return;
        }
        if parts.len() > 3000 {
            self.error = Some(format!("Получится {} кусков - слишком много, увеличьте длину", parts.len()));
            return;
        }
        self.rows = parts
            .into_iter()
            .map(|(a, b)| Row {
                start: timecode::format(a),
                end: timecode::format(b),
                name: String::new(),
            })
            .collect();
        self.save_settings();
    }

    fn draw_template(&mut self, ui: &mut egui::Ui, idle: bool) {
        let egui_ctx = ui.ctx().clone();
        ui.add_enabled_ui(idle, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Шаблон:").strong());
                ui.label("куски по");
                let presets: [(&str, &str); 7] = [
                    ("5", "5 с"),
                    ("10", "10 с"),
                    ("15", "15 с"),
                    ("20", "20 с"),
                    ("30", "30 с"),
                    ("60", "1 мин"),
                    ("300", "5 мин"),
                ];
                for (value, label) in presets {
                    let selected = self.settings.split_step.trim() == value;
                    if ui.selectable_label(selected, label).clicked() {
                        self.settings.split_step = value.to_string();
                        self.request_split(&egui_ctx);
                    }
                }
                ui.label("или");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.split_step)
                        .desired_width(50.0)
                        .hint_text("сек"),
                );
                ui.label("с");
                ui.add(egui::TextEdit::singleline(&mut self.split_from).desired_width(70.0).hint_text("начала"));
                ui.label("по");
                ui.add(egui::TextEdit::singleline(&mut self.split_to).desired_width(70.0).hint_text("конец"));
                if ui.button("Разбить").clicked() {
                    self.request_split(&egui_ctx);
                }
            });
        });
        ui.label(
            RichText::new("Длину куска можно писать в секундах или как 1:30. Пустые \"с\" и \"по\" - всё видео.")
                .weak()
                .small(),
        );
    }

    fn add_bulk(&mut self) {
        let duration = self.info.as_ref().and_then(|i| i.duration);
        let items = timecode::parse_list(&self.bulk_text, duration);
        if items.is_empty() {
            self.error = Some("В тексте не нашлось таймкодов вида 1:23 или 01:02:03".into());
            return;
        }
        self.rows.retain(|r| !r.is_blank());
        for it in items {
            self.rows.push(Row {
                start: timecode::format(it.start),
                end: it.end.map(timecode::format).unwrap_or_default(),
                name: it.name,
            });
        }
        self.bulk_text.clear();
        self.bulk_open = false;
        self.error = None;
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        let egui_ctx = ui.ctx().clone();
        let idle = self.busy == Busy::No;
        let can_work = idle && self.tools_ready;

        ui.heading("Скачать и нарезать видео с YouTube");
        ui.add_space(8.0);

        // ссылка
        ui.horizontal(|ui| {
            ui.label("Ссылка:");
            let edit = egui::TextEdit::singleline(&mut self.url)
                .hint_text("https://www.youtube.com/watch?v=...")
                .desired_width(ui.available_width() - 190.0);
            let resp = ui.add_enabled(idle, edit);
            if resp.changed() && self.info.as_ref().is_some_and(|i| i.url != self.url.trim()) {
                self.info = None;
            }
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let btn = ui.add_enabled(
                can_work && !self.url.trim().is_empty(),
                egui::Button::new("Загрузить информацию"),
            );
            if (btn.clicked() || enter) && can_work {
                self.fetch_info(&egui_ctx);
            }
        });

        if let Some(info) = &self.info {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&info.title).strong());
                if let Some(d) = info.duration {
                    ui.label(format!("длительность {}", timecode::format(d)));
                }
            });
        }

        ui.add_space(10.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Отрезки").strong());
            ui.label(
                RichText::new("время: 83, 1:23 или 1:02:03. Пустой конец - до конца видео")
                    .weak()
                    .small(),
            );
        });
        ui.add_space(4.0);
        self.draw_template(ui, idle);
        ui.add_space(6.0);

        self.draw_rows(ui, idle);

        ui.horizontal(|ui| {
            ui.add_enabled_ui(idle, |ui| {
                if ui.button("Добавить отрезок").clicked() {
                    let start = self.rows.last().map(|r| r.end.clone()).unwrap_or_default();
                    self.rows.push(Row { start, ..Default::default() });
                }
                if ui.button("Вставить списком").clicked() {
                    self.bulk_open = !self.bulk_open;
                }
                let chapters = self.info.as_ref().map(|i| i.chapters.len()).unwrap_or(0);
                if chapters > 0 && ui.button(format!("Взять главы из видео ({chapters})")).clicked() {
                    self.rows_from_chapters();
                }
                if ui.button("Очистить").clicked() {
                    self.rows = vec![Row::default()];
                }
            });
        });

        if self.bulk_open {
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "По одному отрезку на строку: \"0:10-0:45 Название\" или \"12:30 Название\" \
                     (как таймкоды в описании на YouTube - конец берётся из следующей строки)",
                )
                .small(),
            );
            ui.add(
                egui::TextEdit::multiline(&mut self.bulk_text)
                    .desired_rows(5)
                    .desired_width(f32::INFINITY)
                    .hint_text("0:00 Вступление\n2:15 Основная часть\n10:40 Итоги"),
            );
            ui.horizontal(|ui| {
                if ui.button("Добавить в список").clicked() {
                    self.add_bulk();
                }
                if ui.button("Закрыть").clicked() {
                    self.bulk_open = false;
                }
            });
        }

        ui.add_space(10.0);
        ui.separator();
        self.draw_settings(ui, idle);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if self.busy == Busy::Job {
                if ui.add_sized([200.0, 32.0], egui::Button::new("Отмена")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                    self.status = "Отменяю...".into();
                }
            } else {
                let ok = can_work && !self.url.trim().is_empty();
                let btn = egui::Button::new(RichText::new("Скачать и нарезать").strong().size(16.0));
                if ui.add_enabled(ok, btn.min_size(egui::vec2(200.0, 32.0))).clicked() {
                    self.start_job(&egui_ctx);
                }
            }
            if self.busy == Busy::Info {
                ui.spinner();
            }
            if !self.status.is_empty() {
                ui.label(&self.status);
            }
        });

        if let Some(p) = self.progress {
            if self.busy != Busy::No {
                ui.add(egui::ProgressBar::new(p).show_percentage());
            }
        }

        if let Some(e) = self.error.clone() {
            ui.add_space(6.0);
            ui.colored_label(Color32::from_rgb(220, 70, 60), e);
            if !self.tools_ready && idle && ui.button("Повторить загрузку компонентов").clicked() {
                self.prepare_tools(&egui_ctx);
            }
        }

        if let Some((files, dir)) = &self.result {
            ui.add_space(6.0);
            ui.colored_label(
                Color32::from_rgb(60, 170, 90),
                format!("Готово, сохранено файлов: {}", files.len()),
            );
            for f in files.iter().take(30) {
                ui.label(RichText::new(f.file_name().unwrap_or_default().to_string_lossy()).small());
            }
            if ui.button("Открыть папку").clicked() {
                open_folder(dir);
            }
        }

        ui.add_space(10.0);
        egui::CollapsingHeader::new("Подробный журнал").show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for l in &self.log {
                        ui.label(RichText::new(l).monospace().small());
                    }
                });
        });
    }

    fn draw_rows(&mut self, ui: &mut egui::Ui, idle: bool) {
        let duration = self.info.as_ref().and_then(|i| i.duration);
        let mut remove = None;
        let bad = Color32::from_rgb(220, 70, 60);
        egui::Grid::new("segments").num_columns(6).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("#").weak());
            ui.label(RichText::new("Начало").weak());
            ui.label(RichText::new("Конец").weak());
            ui.label(RichText::new("Название (необязательно)").weak());
            ui.label(RichText::new("Длина").weak());
            ui.label("");
            ui.end_row();

            for (i, r) in self.rows.iter_mut().enumerate() {
                let start = timecode::parse(&r.start);
                let end = if r.end.trim().is_empty() { duration } else { timecode::parse(&r.end) };
                let start_bad = !r.start.trim().is_empty() && start.is_none();
                let end_bad = !r.end.trim().is_empty() && end.is_none();

                ui.label(format!("{}", i + 1));
                let mut s = egui::TextEdit::singleline(&mut r.start).desired_width(90.0).hint_text("0:00");
                if start_bad {
                    s = s.text_color(bad);
                }
                ui.add_enabled(idle, s);
                let mut e = egui::TextEdit::singleline(&mut r.end).desired_width(90.0).hint_text("конец");
                if end_bad {
                    e = e.text_color(bad);
                }
                ui.add_enabled(idle, e);
                ui.add_enabled(idle, egui::TextEdit::singleline(&mut r.name).desired_width(260.0));
                match (start, end) {
                    (Some(a), Some(b)) if b > a => ui.label(timecode::format(b - a)),
                    (Some(_), Some(_)) => ui.colored_label(bad, "конец раньше начала"),
                    _ => ui.label(""),
                };
                if ui.add_enabled(idle, egui::Button::new("Удалить").small()).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.rows.remove(i);
            if self.rows.is_empty() {
                self.rows.push(Row::default());
            }
        }
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui, idle: bool) {
        let before = (
            self.settings.quality,
            self.settings.precise,
            self.settings.subfolder,
            self.settings.keep_full,
            self.settings.out_dir.clone(),
        );
        ui.add_enabled_ui(idle, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Качество:");
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(self.settings.quality.label())
                    .show_ui(ui, |ui| {
                        for q in Quality::ALL {
                            ui.selectable_value(&mut self.settings.quality, q, q.label());
                        }
                    });
                ui.end_row();

                ui.label("Нарезка:");
                ui.vertical(|ui| {
                    ui.radio_value(
                        &mut self.settings.precise,
                        true,
                        "Точная, до кадра (с перекодированием, медленнее)",
                    );
                    ui.radio_value(
                        &mut self.settings.precise,
                        false,
                        "Быстрая, без перекодирования (начало может сдвинуться на пару секунд)",
                    );
                });
                ui.end_row();

                ui.label("Сохранять в:");
                ui.horizontal(|ui| {
                    let text = self
                        .settings
                        .out_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "папка не выбрана".into());
                    ui.label(RichText::new(text).monospace());
                    if ui.button("Выбрать...").clicked() {
                        let mut dlg = rfd::FileDialog::new();
                        if let Some(d) = &self.settings.out_dir {
                            dlg = dlg.set_directory(d);
                        }
                        if let Some(p) = dlg.pick_folder() {
                            self.settings.out_dir = Some(p);
                        }
                    }
                    if let Some(d) = &self.settings.out_dir {
                        if ui.button("Открыть").clicked() {
                            open_folder(d);
                        }
                    }
                });
                ui.end_row();

                ui.label("");
                ui.vertical(|ui| {
                    ui.checkbox(&mut self.settings.subfolder, "Создать отдельную папку с названием видео");
                    ui.checkbox(&mut self.settings.keep_full, "Сохранить также полное видео");
                });
                ui.end_row();
            });
        });
        let after = (
            self.settings.quality,
            self.settings.precise,
            self.settings.subfolder,
            self.settings.keep_full,
            self.settings.out_dir.clone(),
        );
        if before != after {
            self.save_settings();
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.draw(ui);
            });
        });
    }
}

fn open_folder(dir: &Path) {
    #[cfg(windows)]
    let prog = "explorer";
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let prog = "xdg-open";
    let _ = std::process::Command::new(prog).arg(dir).spawn();
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([900.0, 760.0])
            .with_min_inner_size([680.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
