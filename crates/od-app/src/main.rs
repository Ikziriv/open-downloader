use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, App, AppContext, Application, AsyncApp, Bounds, Context, FocusHandle,
    FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Path, PathBuilder,
    Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled, TitlebarOptions,
    WeakEntity, Window, WindowAppearance, WindowBounds, WindowOptions, canvas, div, point, px,
    relative, rgb, size,
};
use od_core::{
    AppConfig, CancellationToken, Category, ConnectionQuality, DetectedDownloader, DetectedUrl,
    DownloadSpec, Event, FinishReason, NinjaAction, NinjaDetective, NinjaMode, SpeedTestConfig,
    SpeedTestEvent, SpeedTestPhase, SpeedTestReport, StreamKind, ThemeMode, run_speedtest,
};
use tokio::sync::mpsc;

/// A URL handed to the app — from the input field or the browser extension.
type InboxItem = (String, Option<String>, Vec<(String, String)>);

// 4-tier Obsidian Cobalt (dark) / Porcelain Slate (light) surface scale.
#[derive(Clone, Copy)]
struct Theme {
    bg: u32,
    sidebar: u32,
    card: u32,
    elevated: u32,
    border: u32,
    border_hover: u32,
    text: u32,
    muted: u32,
    accent: u32,
    accent_hover: u32,
    accent_soft: u32,
    track: u32,
    /// Hover bg for neutral buttons.
    hover: u32,
    /// Hover bg for the destructive (✕) button.
    danger_hover: u32,
}

const LIGHT: Theme = Theme {
    bg: 0xF2F5FA,
    sidebar: 0xFFFFFF,
    card: 0xFFFFFF,
    elevated: 0xEEF2F8,
    border: 0xE1E6F0,
    border_hover: 0xC7D0E2,
    text: 0x161D2C,
    muted: 0x5E6A82,
    accent: 0x3B6EF0,
    accent_hover: 0x2B5CE0,
    accent_soft: 0xE8EEFE,
    track: 0xE5EAF3,
    hover: 0xDCE3F0,
    danger_hover: 0xFCE0E0,
};

const DARK: Theme = Theme {
    bg: 0x0B0E14,
    sidebar: 0x10141D,
    card: 0x151B26,
    elevated: 0x1C2433,
    border: 0x232C3E,
    border_hover: 0x33415C,
    text: 0xEDF1F7,
    muted: 0x8E99B0,
    accent: 0x4F7EF2,
    accent_hover: 0x638EF7,
    accent_soft: 0x182544,
    track: 0x263147,
    hover: 0x252F42,
    danger_hover: 0x3E1E27,
};

// Status colors work in both themes.
const GREEN: u32 = 0x16a34a;
const AMBER: u32 = 0xd97706;
const RED: u32 = 0xdc2626;

static DARK_MODE: AtomicBool = AtomicBool::new(false);

fn th() -> Theme {
    if DARK_MODE.load(Ordering::Relaxed) { DARK } else { LIGHT }
}

fn theme_path() -> PathBuf {
    od_core::auth_token_path().with_file_name("theme.txt")
}

fn parse_theme_pref(s: &str) -> Option<bool> {
    match s.trim() {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    }
}

fn load_theme_pref() -> Option<bool> {
    parse_theme_pref(&std::fs::read_to_string(theme_path()).ok()?)
}

fn set_dark(dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
    let path = theme_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, if dark { "dark" } else { "light" });
}

/// Minimalist stroke icons, drawn with `canvas` paths (Feather-style, 24×24 grid).
#[derive(Clone, Copy, PartialEq)]
enum Icon {
    Brand,
    Arrow,
    Play,
    Note,
    Doc,
    Img,
    Zip,
    App,
    File,
    Contrast,
}

// (Category, icon, tile bg, fg) — sidebar and row icons share these.
// Dark variants keep the hue but dim the bg and brighten the fg.
const TILES: [(Category, Icon, u32, u32); 6] = [
    (Category::Video, Icon::Play, 0xdbeafe, 0x2563eb),
    (Category::Audio, Icon::Note, 0xfce7f3, 0xdb2777),
    (Category::Documents, Icon::Doc, 0xfef3c7, 0xb45309),
    (Category::Images, Icon::Img, 0xffe4e6, 0xe11d48),
    (Category::Compressed, Icon::Zip, 0xdcfce7, 0x16a34a),
    (Category::Programs, Icon::App, 0xe0e7ff, 0x4f46e5),
];
const TILES_DARK: [(Category, Icon, u32, u32); 6] = [
    (Category::Video, Icon::Play, 0x15263f, 0x6ea4ff),
    (Category::Audio, Icon::Note, 0x3b1730, 0xf472a6),
    (Category::Documents, Icon::Doc, 0x3a2d10, 0xf0b453),
    (Category::Images, Icon::Img, 0x3a1722, 0xf0617f),
    (Category::Compressed, Icon::Zip, 0x14351f, 0x4ad07a),
    (Category::Programs, Icon::App, 0x1e2342, 0x8a93f5),
];

fn tile_for(cat: Category) -> (Icon, u32, u32) {
    let tiles = if DARK_MODE.load(Ordering::Relaxed) {
        &TILES_DARK
    } else {
        &TILES
    };
    tiles
        .iter()
        .find(|(c, _, _, _)| *c == cat)
        .map(|(_, g, b, f)| (*g, *b, *f))
        .unwrap_or((Icon::File, th().track, th().muted))
}

enum Status {
    Downloading,
    Paused,
    Done,
    Failed(String),
}

struct Row {
    spec: DownloadSpec,
    name: String,
    downloaded: u64,
    total: Option<u64>,
    status: Status,
    cancel: Option<CancellationToken>,
    /// Smoothed bytes/sec from progress deltas.
    speed: f64,
    last_progress: Option<(u64, Instant)>,
    /// Rendered bar fraction, eased toward the real one by the tick task.
    disp_frac: f32,
}

impl Row {
    fn category(&self) -> Category {
        Category::detect(
            &self.name,
            StreamKind::sniff(&self.spec.url, None),
            None,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Active,
    Paused,
    Done,
    Failed,
}

impl StatusFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Paused => "Paused",
            Self::Done => "Done",
            Self::Failed => "Failed",
        }
    }

    fn matches(self, s: &Status) -> bool {
        match self {
            Self::All => true,
            Self::Active => matches!(s, Status::Downloading),
            Self::Paused => matches!(s, Status::Paused),
            Self::Done => matches!(s, Status::Done),
            Self::Failed => matches!(s, Status::Failed(_)),
        }
    }
}

enum SpeedTestMsg {
    Event(SpeedTestEvent),
    Failed(String),
    Cancelled,
}

#[derive(Default)]
struct SpeedTestUi {
    open: bool,
    running: bool,
    phase: Option<SpeedTestPhase>,
    live_mbps: f64,
    live_samples: Vec<f64>,
    report: Option<SpeedTestReport>,
    error: Option<String>,
    cancel: Option<CancellationToken>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Downloads,
    Ninja,
    Startup,
    Security,
    Appearance,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Downloads => "Library & Output",
            Self::Ninja => "Ninja Detective",
            Self::Startup => "Startup",
            Self::Security => "Security",
            Self::Appearance => "Appearance",
        }
    }
}

struct DownloadsView {
    rows: Vec<Row>,
    next_id: u64,
    client: reqwest::Client,
    rt: Arc<tokio::runtime::Runtime>,
    tx: mpsc::UnboundedSender<Event>,
    speed_tx: mpsc::UnboundedSender<SpeedTestMsg>,
    speedtest: SpeedTestUi,
    config: AppConfig,
    settings_open: bool,
    settings_tab: SettingsTab,
    ninja: NinjaDetective,
    ninja_queue: Vec<DetectedUrl>,
    last_clipboard: String,
    other_downloaders: Vec<DetectedDownloader>,
    url_input: String,
    url_focus: FocusHandle,
    search_input: String,
    search_focus: FocusHandle,
    filter: Option<Category>,
    status_filter: StatusFilter,
    /// (is_error, message) — the status bar tints the dot by kind.
    notice: Option<(bool, String)>,
    /// Autofocused the URL field once after first render.
    did_autofocus: bool,
    /// Drives the indeterminate progress animation.
    tick: u64,
    /// Continuous animation phase (marquee sweep, shimmer).
    phase: f32,
    /// Donut's rendered fraction, eased toward the real one.
    ring_disp: f32,
    /// Rolling 32-sample aggregate speed history for the telemetry sparkline.
    speed_history: Vec<f64>,
}

impl DownloadsView {
    fn new(
        client: reqwest::Client,
        rt: Arc<tokio::runtime::Runtime>,
        mut rx: mpsc::UnboundedReceiver<Event>,
        tx: mpsc::UnboundedSender<Event>,
        mut inbox: mpsc::UnboundedReceiver<InboxItem>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (speed_tx, mut speed_rx) = mpsc::unbounded_channel::<SpeedTestMsg>();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Some(msg) = speed_rx.recv().await {
                let _ = this.update(cx, |view, cx| {
                    view.on_speedtest_msg(msg);
                    cx.notify();
                });
            }
        })
        .detach();
        // Pump engine events (tokio runtime) into the GPUI view.
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |view, cx| {
                    view.on_event(ev);
                    cx.notify();
                });
            }
        })
        .detach();
        // Pump URLs arriving from the browser extension (localhost /add).
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Some((url, filename, headers)) = inbox.recv().await {
                let _ = this.update(cx, |view, cx| {
                    view.start_download(url, filename, headers);
                    cx.notify();
                });
            }
        })
        .detach();
        // Animation ticker: marquee sweep, shimmer, eased bars + donut.
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                let alive = this
                    .update(cx, |view, cx| {
                        view.tick = view.tick.wrapping_add(1);
                        view.phase = (view.phase + 0.055) % 10_000.0;
                        let mut busy = false;
                        for r in &mut view.rows {
                            let target = match r.total {
                                Some(t) if t > 0 => {
                                    (r.downloaded as f32 / t as f32).clamp(0.0, 1.0)
                                }
                                _ => 0.0,
                            };
                            r.disp_frac += (target - r.disp_frac) * 0.2;
                            busy |= matches!(r.status, Status::Downloading);
                        }
                        let target = view.agg_frac();
                        view.ring_disp += (target - view.ring_disp) * 0.15;
                        // Sample aggregate speed every ~400ms (5 ticks) for the sparkline.
                        if view.tick.is_multiple_of(5) {
                            let (spd, _, _) = view.aggregate();
                            view.speed_history.push(spd);
                            if view.speed_history.len() > 32 {
                                view.speed_history.remove(0);
                            }
                        }
                        // Background Ninja Detective clipboard scan every ~640ms (8 ticks).
                        let mut ninja_hit = false;
                        if view.tick.is_multiple_of(8)
                            && view.config.ninja.mode != NinjaMode::Off
                            && view.config.ninja.watch_clipboard
                            && let Some(text) = cx.read_from_clipboard().and_then(|i| i.text())
                            && text != view.last_clipboard
                        {
                            view.last_clipboard = text.clone();
                            let hits = view.ninja.inspect_text(
                                &text,
                                "clipboard",
                                &view.config.ninja,
                            );
                            for hit in hits {
                                ninja_hit = true;
                                match view.config.ninja.action {
                                    NinjaAction::AutoDownload => {
                                        view.start_download(hit.url, Some(hit.filename), vec![]);
                                    }
                                    NinjaAction::GrabberQueue => {
                                        view.notice = Some((
                                            false,
                                            format!("Ninja detected {} ({})", hit.filename, hit.reason),
                                        ));
                                        view.ninja_queue.insert(0, hit);
                                        if view.ninja_queue.len() > 8 {
                                            view.ninja_queue.truncate(8);
                                        }
                                    }
                                }
                            }
                        }
                        if busy || ninja_hit || (target - view.ring_disp).abs() > 0.002 {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        Self {
            rows: Vec::new(),
            next_id: 0,
            client,
            rt,
            tx,
            speed_tx,
            speedtest: SpeedTestUi::default(),
            config: AppConfig::load(),
            settings_open: false,
            settings_tab: SettingsTab::Downloads,
            ninja: NinjaDetective::default(),
            ninja_queue: Vec::new(),
            last_clipboard: String::new(),
            other_downloaders: Vec::new(),
            url_input: String::new(),
            url_focus: cx.focus_handle(),
            search_input: String::new(),
            search_focus: cx.focus_handle(),
            filter: None,
            status_filter: StatusFilter::All,
            notice: None,
            did_autofocus: false,
            tick: 0,
            phase: 0.0,
            ring_disp: 0.0,
            speed_history: vec![0.0; 32],
        }
    }

    fn start_speedtest(&mut self) {
        if self.speedtest.running {
            return;
        }
        let cancel = CancellationToken::new();
        self.speedtest.open = true;
        self.speedtest.running = true;
        self.speedtest.phase = Some(SpeedTestPhase::Connecting);
        self.speedtest.live_mbps = 0.0;
        self.speedtest.live_samples.clear();
        self.speedtest.error = None;
        self.speedtest.cancel = Some(cancel.clone());

        let client = self.client.clone();
        let out_tx = self.speed_tx.clone();
        self.rt.spawn(async move {
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<SpeedTestEvent>();
            let fwd_tx = out_tx.clone();
            let fwd = tokio::spawn(async move {
                while let Some(ev) = ev_rx.recv().await {
                    let _ = fwd_tx.send(SpeedTestMsg::Event(ev));
                }
            });
            let cfg = SpeedTestConfig::default();
            let res = run_speedtest(&client, &cfg, cancel, Some(ev_tx)).await;
            let _ = fwd.await;
            match res {
                Ok(_) => {}
                Err(od_core::Error::Cancelled) => {
                    let _ = out_tx.send(SpeedTestMsg::Cancelled);
                }
                Err(e) => {
                    let _ = out_tx.send(SpeedTestMsg::Failed(e.to_string()));
                }
            }
        });
    }

    fn cancel_speedtest(&mut self) {
        if let Some(tok) = self.speedtest.cancel.take() {
            tok.cancel();
        }
    }

    fn on_speedtest_msg(&mut self, msg: SpeedTestMsg) {
        match msg {
            SpeedTestMsg::Event(SpeedTestEvent::PhaseChanged { phase }) => {
                self.speedtest.phase = Some(phase);
            }
            SpeedTestMsg::Event(SpeedTestEvent::Sample { mbps, .. }) => {
                self.speedtest.live_mbps = mbps;
                self.speedtest.live_samples.push(mbps);
                if self.speedtest.live_samples.len() > 32 {
                    self.speedtest.live_samples.remove(0);
                }
            }
            SpeedTestMsg::Event(SpeedTestEvent::Completed { report }) => {
                self.speedtest.running = false;
                self.speedtest.cancel = None;
                self.speedtest.phase = Some(SpeedTestPhase::Done);
                self.speedtest.live_mbps = report.download_mbps;
                self.speedtest.live_samples = report.samples_mbps.clone();
                self.speedtest.report = Some(*report);
            }
            SpeedTestMsg::Cancelled => {
                self.speedtest.running = false;
                self.speedtest.cancel = None;
                self.speedtest.phase = None;
            }
            SpeedTestMsg::Failed(err) => {
                self.speedtest.running = false;
                self.speedtest.cancel = None;
                self.speedtest.phase = None;
                self.speedtest.error = Some(err);
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        let (id, progress) = match &ev {
            Event::Progress {
                id,
                downloaded,
                total,
            } => (*id, Some((*downloaded, *total))),
            Event::Finished { id, .. } => (*id, None),
        };
        let Some(row) = self.rows.iter_mut().find(|r| r.spec.id == id) else {
            return;
        };
        match ev {
            Event::Progress { .. } => {
                let (downloaded, total) = progress.unwrap();
                let now = Instant::now();
                if let Some((prev, at)) = row.last_progress {
                    let dt = now.duration_since(at).as_secs_f64();
                    if dt > 0.0 && downloaded > prev {
                        let inst = (downloaded - prev) as f64 / dt;
                        row.speed = if row.speed > 0.0 {
                            row.speed * 0.7 + inst * 0.3
                        } else {
                            inst
                        };
                    }
                }
                row.last_progress = Some((downloaded, now));
                row.downloaded = downloaded;
                row.total = total;
            }
            Event::Finished { reason, .. } => {
                row.cancel = None;
                row.speed = 0.0;
                row.status = match reason {
                    FinishReason::Completed(_) => Status::Done,
                    FinishReason::Cancelled => Status::Paused,
                    FinishReason::Failed(e) => Status::Failed(e),
                };
                // Snap the eased bar to its final value — the tick loop
                // stops notifying once nothing is downloading, so without
                // this the bar freezes mid-lerp (and streams, which never
                // learn a total, would render an empty bar marked "done").
                row.disp_frac = match (&row.status, row.total) {
                    (Status::Done, _) => 1.0,
                    (_, Some(t)) if t > 0 => {
                        (row.downloaded as f32 / t as f32).clamp(0.0, 1.0)
                    }
                    _ => row.disp_frac,
                };
            }
        }
    }

    fn start_download(
        &mut self,
        url: String,
        filename: Option<String>,
        headers: Vec<(String, String)>,
    ) {
        // A new row must be visible — reset any active filter/search.
        self.filter = None;
        self.status_filter = StatusFilter::All;
        self.search_input.clear();
        let name = filename
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                url.rsplit('/')
                    .next()
                    .and_then(|s| s.split('?').next())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("download.bin")
                    .to_string()
            });
        self.notice = Some((false, format!("queued {name}")));
        let spec = DownloadSpec {
            id: self.next_id,
            url,
            dest: self.config.downloads.download_dir.join(&name),
            headers,
        };
        self.next_id += 1;
        // Newest on top — IDM-style; the user never scrolls for the row
        // they just added.
        self.rows.insert(
            0,
            Row {
                name,
                spec,
                downloaded: 0,
                total: None,
                status: Status::Downloading,
                cancel: None,
                speed: 0.0,
                last_progress: None,
                disp_frac: 0.0,
            },
        );
        self.launch(0);
    }

    /// (Re)spawn the engine task for `rows[i]`. The journal makes a resume
    /// transparent — cancelled downloads pick up where they stopped.
    fn launch(&mut self, i: usize) {
        let row = &mut self.rows[i];
        let cancel = CancellationToken::new();
        row.cancel = Some(cancel.clone());
        row.status = Status::Downloading;
        row.last_progress = None;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let spec = row.spec.clone();
        let mut opts = self.config.to_download_options();
        opts.cancel = Some(cancel);
        self.rt.spawn(async move {
            od_core::run(&client, spec, opts, tx).await;
        });
    }

    fn toggle(&mut self, id: u64) {
        let Some(i) = self.rows.iter().position(|r| r.spec.id == id) else {
            return;
        };
        match self.rows[i].status {
            Status::Downloading => {
                if let Some(t) = self.rows[i].cancel.take() {
                    t.cancel();
                }
            }
            Status::Paused | Status::Failed(_) => self.launch(i),
            Status::Done => {}
        }
    }

    fn remove(&mut self, id: u64) {
        if let Some(i) = self.rows.iter().position(|r| r.spec.id == id) {
            if let Some(t) = self.rows[i].cancel.take() {
                t.cancel();
            }
            self.rows.remove(i);
        }
    }

    /// Drop finished/failed rows; anything resumable (paused, live) stays.
    fn clear_finished(&mut self) {
        self.rows
            .retain(|r| matches!(r.status, Status::Downloading | Status::Paused));
    }

    fn pause_all(&mut self) {
        for r in &mut self.rows {
            if matches!(r.status, Status::Downloading)
                && let Some(t) = r.cancel.take()
            {
                t.cancel();
            }
        }
    }

    fn resume_all(&mut self) {
        let to_resume: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.status, Status::Paused | Status::Failed(_)))
            .map(|(i, _)| i)
            .collect();
        for i in to_resume {
            self.launch(i);
        }
    }

    fn submit_url(&mut self) {
        let url = self.url_input.trim().to_string();
        if url.starts_with("http://") || url.starts_with("https://") {
            self.url_input.clear();
            self.notice = None;
            self.start_download(url, None, Vec::new());
        } else if !url.is_empty() {
            self.notice = Some((true, "not a valid http(s) URL".into()));
        }
    }

    fn visible_rows(&self) -> impl Iterator<Item = &Row> {
        let query = self.search_input.to_lowercase();
        let st_filter = self.status_filter;
        self.rows.iter().filter(move |r| {
            self.filter.is_none_or(|c| r.category() == c)
                && st_filter.matches(&r.status)
                && (query.is_empty() || r.name.to_lowercase().contains(&query))
        })
    }

    fn aggregate(&self) -> (f64, u64, u64) {
        let speed = self.rows.iter().map(|r| r.speed).sum();
        let done: u64 = self.rows.iter().map(|r| r.downloaded).sum();
        let total: u64 = self
            .rows
            .iter()
            .filter(|r| matches!(r.status, Status::Downloading | Status::Paused))
            .filter_map(|r| r.total)
            .sum();
        (speed, done, total)
    }

    /// Fraction the sidebar donut should show.
    fn agg_frac(&self) -> f32 {
        let (down, total): (u64, u64) = self
            .rows
            .iter()
            .filter(|r| matches!(r.status, Status::Downloading | Status::Paused))
            .fold((0, 0), |(d, t), r| {
                (d + r.downloaded, t + r.total.unwrap_or(0))
            });
        if total > 0 {
            (down as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

// ---------------------------------------------------------------------------
// styling helpers

fn status_color(s: &Status) -> u32 {
    match s {
        Status::Downloading => th().accent,
        Status::Paused => AMBER,
        Status::Done => GREEN,
        Status::Failed(_) => RED,
    }
}

fn status_label(s: &Status) -> &'static str {
    match s {
        Status::Downloading => "downloading",
        Status::Paused => "paused",
        Status::Done => "done",
        Status::Failed(_) => "failed",
    }
}

/// "~42s" / "3m 12s" / "1h 04m" for the ETA readout.
fn human_dur(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{}m {:02.0}s", (secs / 60.0) as u32, secs % 60.0)
    } else {
        format!("{}h {:02.0}m", (secs / 3600.0) as u32, (secs % 3600.0) / 60.0)
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// Colored rounded tile with a vector icon inside — `flex_shrink_0` ensures
/// icons always stay perfectly square regardless of sibling text width.
fn icon_tile(icon: Icon, bg: u32, fg: u32, size: f32) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .rounded_lg()
        .bg(rgb(bg))
        .flex()
        .items_center()
        .justify_center()
        .child(icon_canvas(icon, fg, size * 0.62))
}

fn poly(pts: &[Point<Pixels>], close: bool, stroke: f32) -> Option<Path<Pixels>> {
    let mut b = PathBuilder::stroke(px(stroke));
    b.move_to(pts[0]);
    for p in &pts[1..] {
        b.line_to(*p);
    }
    if close {
        b.close();
    }
    b.build().ok()
}

fn circle_path(c: Point<Pixels>, r: Pixels, stroke: f32) -> Option<Path<Pixels>> {
    let mut b = PathBuilder::stroke(px(stroke));
    b.move_to(point(c.x, c.y - r));
    let radii = point(r, r);
    b.arc_to(radii, px(0.), false, true, point(c.x, c.y + r));
    b.arc_to(radii, px(0.), false, true, point(c.x, c.y - r));
    b.build().ok()
}

/// Stroke-drawn icon on a normalized 24×24 grid, scaled into its bounds.
fn icon_canvas(icon: Icon, fg: u32, size: f32) -> impl IntoElement {
    canvas(
        move |bounds, _w, _cx| bounds,
        move |bounds, _t, window, _cx| {
            let s = bounds.size.width.min(bounds.size.height) / px(24.);
            let ox = bounds.origin.x + (bounds.size.width - px(24.) * s) / 2.0;
            let oy = bounds.origin.y + (bounds.size.height - px(24.) * s) / 2.0;
            let p = |x: f32, y: f32| point(ox + px(x) * s, oy + px(y) * s);
            let w = (1.6 * s).max(1.0);
            let color = rgb(fg);
            let paint = |window: &mut Window, path: Option<Path<Pixels>>| {
                if let Some(path) = path {
                    window.paint_path(path, color);
                }
            };
            match icon {
                Icon::Brand => {
                    // Bespoke geometric brand mark: solid downward chevron-arrow + sleek velocity bar.
                    let mut stem = PathBuilder::fill();
                    stem.move_to(p(10.3, 4.0));
                    stem.line_to(p(13.7, 4.0));
                    stem.line_to(p(13.7, 11.2));
                    stem.line_to(p(17.6, 11.2));
                    stem.line_to(p(12.0, 17.0));
                    stem.line_to(p(6.4, 11.2));
                    stem.line_to(p(10.3, 11.2));
                    stem.close();
                    if let Ok(path) = stem.build() {
                        window.paint_path(path, color);
                    }
                    let mut bar = PathBuilder::fill();
                    bar.move_to(p(5.2, 18.6));
                    bar.line_to(p(18.8, 18.6));
                    bar.line_to(p(18.8, 20.8));
                    bar.line_to(p(5.2, 20.8));
                    bar.close();
                    if let Ok(path) = bar.build() {
                        window.paint_path(path, color);
                    }
                }
                Icon::Arrow => {
                    paint(window, poly(&[p(12., 3.), p(12., 15.)], false, w));
                    paint(
                        window,
                        poly(&[p(7., 10.), p(12., 15.), p(17., 10.)], false, w),
                    );
                    paint(
                        window,
                        poly(
                            &[p(20., 15.), p(20., 20.), p(4., 20.), p(4., 15.)],
                            false,
                            w,
                        ),
                    );
                }
                Icon::Play => {
                    paint(window, circle_path(p(12., 12.), px(10.) * s, w));
                    let mut b = PathBuilder::fill();
                    b.move_to(p(10., 8.));
                    b.line_to(p(17., 12.));
                    b.line_to(p(10., 16.));
                    b.close();
                    if let Ok(path) = b.build() {
                        window.paint_path(path, color);
                    }
                }
                Icon::Note => {
                    paint(window, poly(&[p(9., 18.), p(9., 5.)], false, w));
                    paint(window, poly(&[p(21., 16.), p(21., 3.)], false, w));
                    paint(window, poly(&[p(9., 5.), p(21., 3.)], false, w));
                    paint(window, circle_path(p(6.5, 18.), px(2.5) * s, w));
                    paint(window, circle_path(p(18.5, 16.), px(2.5) * s, w));
                }
                Icon::Doc => {
                    paint(
                        window,
                        poly(
                            &[
                                p(5., 3.),
                                p(14., 3.),
                                p(19., 8.),
                                p(19., 21.),
                                p(5., 21.),
                            ],
                            true,
                            w,
                        ),
                    );
                    paint(
                        window,
                        poly(&[p(14., 3.), p(14., 8.), p(19., 8.)], false, w),
                    );
                }
                Icon::Img => {
                    paint(
                        window,
                        poly(
                            &[p(3., 4.), p(21., 4.), p(21., 20.), p(3., 20.)],
                            true,
                            w,
                        ),
                    );
                    paint(window, circle_path(p(8.5, 8.5), px(1.6) * s, w));
                    paint(
                        window,
                        poly(
                            &[p(4., 19.), p(9.5, 12.5), p(13., 16.), p(16., 13.), p(20., 19.)],
                            false,
                            w,
                        ),
                    );
                }
                Icon::Zip => {
                    paint(
                        window,
                        poly(&[p(2., 4.), p(22., 4.), p(22., 8.), p(2., 8.)], true, w),
                    );
                    paint(
                        window,
                        poly(&[p(4., 8.), p(4., 21.), p(20., 21.), p(20., 8.)], false, w),
                    );
                    paint(window, poly(&[p(10., 12.), p(14., 12.)], false, w));
                }
                Icon::App => {
                    paint(
                        window,
                        poly(&[p(4., 7.), p(10., 12.), p(4., 17.)], false, w),
                    );
                    paint(window, poly(&[p(12., 18.), p(20., 18.)], false, w));
                }
                Icon::File => {
                    paint(
                        window,
                        poly(&[p(7., 3.), p(17., 3.), p(17., 21.), p(7., 21.)], true, w),
                    );
                    paint(window, poly(&[p(9.5, 9.), p(14.5, 9.)], false, w));
                    paint(window, poly(&[p(9.5, 13.), p(14.5, 13.)], false, w));
                }
                Icon::Contrast => {
                    // Theme glyph: ring + filled right half.
                    paint(window, circle_path(p(12., 12.), px(8.) * s, w));
                    let mut b = PathBuilder::fill();
                    b.move_to(p(12., 4.));
                    b.arc_to(point(px(8.) * s, px(8.) * s), px(0.), false, true, p(12., 20.));
                    b.close();
                    if let Ok(path) = b.build() {
                        window.paint_path(path, color);
                    }
                }
            }
        },
    )
    .w(px(size))
    .h(px(size))
}

/// Darken/lighten an rgb color for pressed-state feedback.
fn shade(c: u32, f: f32) -> u32 {
    let ch = |shift: u32| (((c >> shift) & 0xff) as f32 * f).clamp(0.0, 255.0) as u32;
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Blend `c` toward white by `f` (0 = unchanged, 1 = white) — pill tints.
fn tint(c: u32, f: f32) -> u32 {
    let ch = |shift: u32| {
        let v = ((c >> shift) & 0xff) as f32;
        (v + (255.0 - v) * f).clamp(0.0, 255.0) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Small colored badge for a row's status — shows the live percentage
/// while a determinate download runs, the label otherwise.
fn status_pill(s: &Status, frac: f32) -> impl IntoElement {
    let c = status_color(s);
    let label: SharedString = match s {
        Status::Downloading if frac > 0.0 => format!("{:.0}%", frac * 100.0).into(),
        _ => status_label(s).into(),
    };
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(rgb(tint(c, 0.86)))
        .flex_shrink_0()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(shade(c, 0.62)))
        .whitespace_nowrap()
        .child(label)
}

fn btn(
    id: impl Into<gpui::ElementId>,
    label: &str,
    bg: u32,
    fg: u32,
    hover: u32,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
        .text_xs()
        .whitespace_nowrap()
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover)))
        .active(move |s| s.bg(rgb(shade(hover, 0.88))))
        .on_click(on_click)
        .child(SharedString::from(label.to_string()))
}

/// Real pill toggle switch (38×22 track + sliding 16×16 white knob).
fn toggle_switch(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(38.))
        .h(px(22.))
        .min_w(px(38.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(if checked { th().accent } else { th().track }))
        .p(px(3.))
        .flex()
        .items_center()
        .when(checked, |d| d.justify_end())
        .when(!checked, |d| d.justify_start())
        .cursor_pointer()
        .on_click(on_click)
        .child(
            div()
                .w(px(16.))
                .h(px(16.))
                .rounded_full()
                .bg(rgb(0xffffff)),
        )
}

/// Consistent editorial setting row: bold title + 1-line helper description + right control.
fn setting_row(
    title: &'static str,
    desc: impl Into<SharedString>,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_4()
        .py_3()
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(px(0.))
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(th().muted))
                        .child(desc.into()),
                ),
        )
        .child(div().flex_shrink_0().child(control))
}

fn setting_divider() -> impl IntoElement {
    div().h(px(1.)).w_full().bg(rgb(th().border))
}

/// A progress donut painted with `canvas`: gray track ring + accent arc.
fn ring(frac: f32, diameter: f32, stroke: f32) -> impl IntoElement {
    let frac = frac.clamp(0.0, 1.0);
    canvas(
        move |bounds, _window, _cx| bounds,
        move |bounds, _t, window, _cx| {
            let c = bounds.center();
            let r = (bounds.size.width.min(bounds.size.height) / 2.0) - px(stroke / 2.0);
            if r <= px(0.) {
                return;
            }
            let top = point(c.x, c.y - r);
            let bottom = point(c.x, c.y + r);
            let radii = point(r, r);
            // Track: full circle as two 180° arcs.
            let mut b = PathBuilder::stroke(px(stroke));
            b.move_to(top);
            b.arc_to(radii, px(0.), false, true, bottom);
            b.arc_to(radii, px(0.), false, true, top);
            if let Ok(p) = b.build() {
                window.paint_path(p, rgb(th().track));
            }
            if frac <= 0.0 {
                return;
            }
            // Progress arc from the top, clockwise, frac * 360°.
            let f = frac.min(0.9999);
            let angle = f * 2.0 * std::f32::consts::PI;
            let end = point(c.x + r * angle.sin(), c.y - r * angle.cos());
            let mut b = PathBuilder::stroke(px(stroke));
            b.move_to(top);
            b.arc_to(radii, px(0.), f > 0.5, true, end);
            if let Ok(p) = b.build() {
                window.paint_path(p, rgb(th().accent));
            }
        },
    )
    .w(px(diameter))
    .h(px(diameter))
}

/// Live throughput sparkline rendered directly on GPUI `canvas`.
fn sparkline_canvas(samples: Vec<f64>, width: f32, height: f32) -> impl IntoElement {
    canvas(
        move |bounds, _w, _cx| bounds,
        move |bounds, _t, window, _cx| {
            if samples.len() < 2 {
                return;
            }
            let max_v = samples
                .iter()
                .copied()
                .fold(1.0_f64, f64::max);
            let w = bounds.size.width;
            let h = bounds.size.height;
            let pad_y = px(4.);
            let usable_h = (h - pad_y * 2.0).max(px(1.));
            let step_x = w / (samples.len() - 1) as f32;

            let mut pts = Vec::with_capacity(samples.len());
            for (i, &val) in samples.iter().enumerate() {
                let frac = (val / max_v).clamp(0.0, 1.0) as f32;
                let x = bounds.origin.x + step_x * i as f32;
                let y = bounds.origin.y + h - pad_y - usable_h * frac;
                pts.push(point(x, y));
            }
            if let Some(path) = poly(&pts, false, 1.8) {
                window.paint_path(path, rgb(th().accent));
            }
        },
    )
    .w(px(width))
    .h(px(height))
}

fn protocol_tag(url: &str) -> (&'static str, bool) {
    let lower = url.trim().to_ascii_lowercase();
    if lower.contains(".m3u8") {
        ("HLS", true)
    } else if lower.contains(".mpd") {
        ("DASH", true)
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        ("HTTP", true)
    } else {
        ("URL", false)
    }
}

fn stat_card(
    label: &str,
    value: String,
    badge: Option<(String, u32)>,
    id: &'static str,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_1()
        .flex()
        .items_center()
        .justify_between()
        .px_3p5()
        .py_2p5()
        .rounded_xl()
        .bg(rgb(th().card))
        .border_1()
        .border_color(rgb(th().border))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(th().muted))
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .child(value),
                ),
        )
        .when_some(badge, |d, (text, color)| {
            d.child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(rgb(tint(color, 0.86)))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(shade(color, 0.62)))
                    .whitespace_nowrap()
                    .child(text),
            )
        })
}

fn speed_stat_card(value: String, samples: Vec<f64>) -> impl IntoElement {
    div()
        .id("s-speed")
        .flex_1()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_3p5()
        .py_2p5()
        .rounded_xl()
        .bg(rgb(th().card))
        .border_1()
        .border_color(rgb(th().border))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(th().muted))
                        .child("THROUGHPUT"),
                )
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .child(value),
                ),
        )
        .child(sparkline_canvas(samples, 92., 34.))
}

fn input_box(
    id: &'static str,
    focus: &FocusHandle,
    focused: bool,
    text: &str,
    placeholder: &'static str,
    on_key: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (shown, dim): (SharedString, bool) = if text.is_empty() {
        (placeholder.into(), true)
    } else {
        (text.to_string().into(), false)
    };
    let f = focus.clone();
    div()
        .id(id)
        .track_focus(focus)
        .flex_1()
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(rgb(th().card))
        .border_1()
        .border_color(rgb(if focused { th().accent } else { th().border }))
        .when(focused, |d| d.shadow_md())
        .text_sm()
        .when(dim, |d| d.text_color(rgb(th().muted)))
        .cursor_text()
        .on_mouse_down(MouseButton::Left, move |_ev, window, _cx| f.focus(window))
        .on_key_down(on_key)
        .child(shown)
}

fn sidebar_item(
    cat: Option<Category>,
    idx: usize,
    label: &'static str,
    count: usize,
    active: bool,
    cx: &Context<DownloadsView>,
) -> impl IntoElement {
    let (icon, tile_bg, tile_fg) = match cat {
        Some(c) => tile_for(c),
        None => (Icon::Arrow, th().accent_soft, th().accent),
    };
    div()
        .id(("nav", idx))
        .relative()
        .flex()
        .items_center()
        .gap_2()
        .mx_2()
        .px_2p5()
        .py_1p5()
        .rounded_lg()
        .cursor_pointer()
        .when(active, |d| d.bg(rgb(th().accent_soft)))
        .hover(|s| s.bg(rgb(if active { th().accent_soft } else { th().elevated })))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _e, _w, cx| {
                view.filter = cat;
                cx.notify();
            }),
        )
        .when(active, |d| {
            d.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(6.))
                    .bottom(px(6.))
                    .w(px(3.))
                    .rounded_full()
                    .bg(rgb(th().accent)),
            )
        })
        .child(icon_tile(icon, tile_bg, tile_fg, 24.))
        .child(
            div()
                .flex_1()
                .text_sm()
                .when(active, |d| {
                    d.font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(th().accent))
                })
                .child(label),
        )
        .child(
            div()
                .px_1p5()
                .py_0p5()
                .rounded_full()
                .bg(rgb(if active { th().accent } else { th().elevated }))
                .text_xs()
                .text_color(rgb(if active { 0xffffff } else { th().muted }))
                .child(count.to_string()),
        )
}

// ---------------------------------------------------------------------------
// render

impl Render for DownloadsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.did_autofocus {
            self.did_autofocus = true;
            self.url_focus.focus(window);
            // Saved preference wins; otherwise follow the OS appearance.
            let dark = load_theme_pref()
                .unwrap_or_else(|| matches!(window.appearance(), WindowAppearance::Dark));
            DARK_MODE.store(dark, Ordering::Relaxed);
        }
        let url_focused = self.url_focus.is_focused(window);
        let search_focused = self.search_focus.is_focused(window);
        let (speed, done_bytes, _pending_total) = self.aggregate();
        let frac = self.ring_disp;
        let counts = |c: Option<Category>| {
            self.rows
                .iter()
                .filter(|r| c.is_none_or(|c| r.category() == c))
                .count()
        };
        let active = self
            .rows
            .iter()
            .filter(|r| matches!(r.status, Status::Downloading))
            .count();
        let finished = self
            .rows
            .iter()
            .filter(|r| matches!(r.status, Status::Done))
            .count();

        // Sidebar
        let nav = std::iter::once(sidebar_item(None, 0, "All Downloads", counts(None), self.filter.is_none(), cx))
            .chain(TILES.iter().enumerate().map(|(i, (cat, _, _, _))| {
                sidebar_item(
                    Some(*cat),
                    i + 1,
                    cat.dir_name(),
                    counts(Some(*cat)),
                    self.filter == Some(*cat),
                    cx,
                )
            }));
        let sidebar = div()
            .w(px(236.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(th().sidebar))
            .border_r_1()
            .border_color(rgb(th().border))
            .py_3()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3p5()
                    .pt_1()
                    .pb_4()
                    .mb_1()
                    .border_b_1()
                    .border_color(rgb(th().border))
                    .child(
                        div()
                            .w(px(34.))
                            .h(px(34.))
                            .min_w(px(34.))
                            .min_h(px(34.))
                            .flex_shrink_0()
                            .rounded_xl()
                            .bg(rgb(th().accent))
                            .border_1()
                            .border_color(rgb(th().accent_hover))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon_canvas(Icon::Brand, 0xffffff, 19.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w(px(0.))
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .whitespace_nowrap()
                                    .child("Open Downloader"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .w(px(6.))
                                            .h(px(6.))
                                            .flex_shrink_0()
                                            .rounded_full()
                                            .bg(rgb(GREEN)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(th().muted))
                                            .whitespace_nowrap()
                                            .child(concat!("v", env!("CARGO_PKG_VERSION"), " · Core")),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(th().muted))
                    .child("LIBRARY"),
            )
            .children(nav)
            .child(div().flex_1())
            .child(
                div()
                    .mx_3()
                    .mb_2()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().flex_1().child(btn(
                        "speedtest-open",
                        if self.speedtest.running {
                            "Testing…"
                        } else {
                            "Speed Test"
                        },
                        th().accent_soft,
                        th().accent,
                        th().hover,
                        cx.listener(|v, _e, _w, cx| {
                            v.settings_open = false;
                            if !v.speedtest.open {
                                v.speedtest.open = true;
                                if v.speedtest.report.is_none() && !v.speedtest.running {
                                    v.start_speedtest();
                                }
                            } else {
                                v.speedtest.open = false;
                            }
                            cx.notify();
                        }),
                    )))
                    .child(btn(
                        "settings-open",
                        "Settings",
                        if self.settings_open {
                            th().accent
                        } else {
                            th().elevated
                        },
                        if self.settings_open {
                            0xffffff
                        } else {
                            th().text
                        },
                        if self.settings_open {
                            th().accent_hover
                        } else {
                            th().hover
                        },
                        cx.listener(|v, _e, _w, cx| {
                            v.settings_open = !v.settings_open;
                            if v.settings_open {
                                v.speedtest.open = false;
                            }
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .mx_3()
                    .p_3()
                    .rounded_xl()
                    .bg(rgb(th().card))
                    .border_1()
                    .border_color(rgb(th().border))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(ring(frac, 46., 5.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("{:.0}%", frac * 100.)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(th().muted))
                                    .child(format!("{} saved", human_bytes(done_bytes))),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("theme-toggle")
                            .w(px(28.))
                            .h(px(28.))
                            .rounded_lg()
                            .bg(rgb(th().elevated))
                            .border_1()
                            .border_color(rgb(th().border))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(th().hover)))
                            .on_click(cx.listener(|_v, _e, _w, cx| {
                                set_dark(!DARK_MODE.load(Ordering::Relaxed));
                                cx.notify();
                            }))
                            .child(icon_canvas(Icon::Contrast, th().muted, 14.)),
                    ),
            );

        // Top Command Bar: live protocol pill + URL input + Paste + Download CTA
        let search_focus = self.search_focus.clone();
        let url_focus = self.url_focus.clone();
        let url_empty = self.url_input.is_empty();
        let (proto_label, proto_lit) = protocol_tag(&self.url_input);
        let topbar = div()
            .flex()
            .items_center()
            .gap_2p5()
            .px_4()
            .py_3()
            .bg(rgb(th().sidebar))
            .border_b_1()
            .border_color(rgb(th().border))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(if proto_lit {
                        th().accent_soft
                    } else {
                        th().elevated
                    }))
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(if proto_lit {
                        th().accent
                    } else {
                        th().muted
                    }))
                    .child(proto_label),
            )
            .child(input_box(
                "url-input",
                &url_focus,
                url_focused,
                &self.url_input,
                "Paste or type a URL (HTTP/S, HLS .m3u8, DASH .mpd)…",
                cx.listener(|v, ev: &KeyDownEvent, w, cx| {
                    let ks = &ev.keystroke;
                    match ks.key.as_str() {
                        "backspace" => {
                            v.url_input.pop();
                        }
                        "enter" => v.submit_url(),
                        "tab" => v.search_focus.focus(w),
                        "v" if ks.modifiers.control => {
                            if let Some(t) = cx
                                .read_from_clipboard()
                                .and_then(|i| i.text())
                            {
                                v.url_input.push_str(t.trim());
                            }
                        }
                        _ => {
                            if !(ks.modifiers.control || ks.modifiers.alt)
                                && let Some(ch) = &ks.key_char
                            {
                                v.url_input.push_str(ch);
                            }
                        }
                    }
                    cx.notify();
                }),
            ))
            .child(btn(
                "paste-btn",
                "Paste",
                th().elevated,
                th().text,
                th().hover,
                cx.listener(|view, _ev, _w, cx| {
                    if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        view.url_input = t.trim().to_string();
                        cx.notify();
                    }
                }),
            ))
            .child(btn(
                "add-btn",
                "+ Download",
                th().accent,
                0xffffff,
                th().accent_hover,
                cx.listener(move |view, _ev, _w, cx| {
                    if url_empty {
                        match cx.read_from_clipboard().and_then(|i| i.text()) {
                            Some(t) if t.trim().starts_with("http") => {
                                view.url_input = t.trim().to_string();
                            }
                            Some(_) => {
                                view.notice = Some((
                                    true,
                                    "clipboard doesn't contain a URL".into(),
                                ));
                            }
                            None => {
                                view.notice =
                                    Some((true, "paste or type a URL first".into()));
                            }
                        }
                    }
                    view.submit_url();
                    cx.notify();
                }),
            ));

        // Compact Telemetry Strip with live speed sparkline
        let stats = div()
            .flex()
            .gap_3()
            .px_4()
            .pt_3()
            .pb_2p5()
            .child(stat_card(
                "ACTIVE",
                format!("{active}"),
                Some((
                    if active > 0 {
                        "Transferring".into()
                    } else {
                        "Idle".into()
                    },
                    th().accent,
                )),
                "s-active",
            ))
            .child(stat_card(
                "COMPLETED",
                format!("{finished} files"),
                Some((human_bytes(done_bytes), GREEN)),
                "s-done",
            ))
            .child(speed_stat_card(
                if speed > 0.0 {
                    format!("{}/s", human_bytes(speed as u64))
                } else {
                    "0 B/s".into()
                },
                self.speed_history.clone(),
            ));

        // Downloads Toolbar: Status Filter Pills + Search + Batch Actions
        let phase = self.phase;
        let shown = self.visible_rows().count();
        let rows: Vec<_> = self
            .visible_rows()
            .map(|r| row_view(r, phase, cx))
            .collect();
        let has_paused = self
            .rows
            .iter()
            .any(|r| matches!(r.status, Status::Paused | Status::Failed(_)));
        let clearable = self
            .rows
            .iter()
            .any(|r| matches!(r.status, Status::Done | Status::Failed(_)));

        let status_pills = [
            StatusFilter::All,
            StatusFilter::Active,
            StatusFilter::Paused,
            StatusFilter::Done,
            StatusFilter::Failed,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, sf)| {
            let selected = self.status_filter == sf;
            div()
                .id(("sf", i))
                .px_2p5()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_xs()
                .when(selected, |d| {
                    d.bg(rgb(th().accent_soft))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(th().accent))
                })
                .when(!selected, |d| {
                    d.text_color(rgb(th().muted))
                        .hover(|s| s.bg(rgb(th().elevated)).text_color(rgb(th().text)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _e, _w, cx| {
                        v.status_filter = sf;
                        cx.notify();
                    }),
                )
                .child(sf.label())
        });

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .pb_2p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_0p5()
                            .p_0p5()
                            .rounded_lg()
                            .bg(rgb(th().card))
                            .border_1()
                            .border_color(rgb(th().border))
                            .children(status_pills),
                    )
                    .child(div().w(px(190.)).child(input_box(
                        "search",
                        &search_focus,
                        search_focused,
                        &self.search_input,
                        "Filter downloads…",
                        cx.listener(|v, ev: &KeyDownEvent, w, cx| {
                            let ks = &ev.keystroke;
                            match ks.key.as_str() {
                                "backspace" => {
                                    v.search_input.pop();
                                }
                                "tab" => v.url_focus.focus(w),
                                _ => {
                                    if !(ks.modifiers.control || ks.modifiers.alt)
                                        && let Some(ch) = &ks.key_char
                                    {
                                        v.search_input.push_str(ch);
                                    }
                                }
                            }
                            cx.notify();
                        }),
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(th().muted))
                            .child(format!("{shown} / {}", self.rows.len())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .when(active > 0, |d| {
                        d.child(btn(
                            "pause-all",
                            "Pause All",
                            th().elevated,
                            th().text,
                            th().hover,
                            cx.listener(|v, _e, _w, cx| {
                                v.pause_all();
                                cx.notify();
                            }),
                        ))
                    })
                    .when(has_paused, |d| {
                        d.child(btn(
                            "resume-all",
                            "Resume All",
                            th().elevated,
                            th().text,
                            th().hover,
                            cx.listener(|v, _e, _w, cx| {
                                v.resume_all();
                                cx.notify();
                            }),
                        ))
                    })
                    .when(clearable, |d| {
                        d.child(btn(
                            "clear-fin",
                            "Clear Done",
                            th().elevated,
                            th().text,
                            th().hover,
                            cx.listener(|v, _e, _w, cx| {
                                v.clear_finished();
                                cx.notify();
                            }),
                        ))
                    }),
            );
        let list = div()
            .id("list")
            .flex_1()
            .min_w(px(0.))
            .flex_col()
            .gap_2()
            .px_4()
            .pb_3()
            .overflow_y_scroll()
            .when(rows.is_empty(), |d| {
                let (title, hint) = if self.rows.is_empty() {
                    (
                        "Ready for high-speed downloads",
                        "Paste a URL above (Ctrl+V → Enter), or capture from the browser extension.",
                    )
                } else {
                    (
                        "No matching downloads",
                        "Try another category, status pill, or clear the filter.",
                    )
                };
                let chips = ["HTTP/S Ranged", "HLS .m3u8", "DASH .mpd", "Auto-Retry", "MCP API"]
                    .into_iter()
                    .map(|c| {
                        div()
                            .px_2p5()
                            .py_0p5()
                            .rounded_full()
                            .bg(rgb(th().elevated))
                            .border_1()
                            .border_color(rgb(th().border))
                            .text_xs()
                            .text_color(rgb(th().muted))
                            .child(c)
                    });
                d.child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2p5()
                        .rounded_xl()
                        .bg(rgb(th().card))
                        .border_1()
                        .border_color(rgb(th().border))
                        .p_6()
                        .child(icon_tile(Icon::Arrow, th().accent_soft, th().accent, 46.))
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(th().muted))
                                .child(hint),
                        )
                        .child(div().flex().items_center().gap_1p5().pt_1().children(chips)),
                )
            })
            .children(rows);

        // Status bar
        let status = div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(rgb(th().border))
            .text_xs()
            .text_color(rgb(th().muted))
            .child(match &self.notice {
                Some((err, msg)) => div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(rgb(if *err { RED } else { GREEN })),
                    )
                    .child(msg.clone()),
                None => div().child("ready".to_string()),
            })
            .child(format!("od · 127.0.0.1:{}", od_core::APP_PORT));

        let speed_panel = self.speedtest.open.then(|| {
            let st = &self.speedtest;
            let (badge_txt, badge_col) = if st.running {
                let p = match st.phase {
                    Some(SpeedTestPhase::Ping) => "Measuring Ping…",
                    Some(SpeedTestPhase::Download) => "Testing Download…",
                    Some(SpeedTestPhase::Upload) => "Testing Upload…",
                    _ => "Connecting…",
                };
                (p.to_string(), th().accent)
            } else if st.error.is_some() {
                ("Offline / Error".to_string(), RED)
            } else if let Some(r) = &st.report {
                let col = match r.quality {
                    ConnectionQuality::Excellent | ConnectionQuality::Good => GREEN,
                    ConnectionQuality::Fair => AMBER,
                    ConnectionQuality::Poor | ConnectionQuality::Offline => RED,
                };
                (r.quality.label().to_string(), col)
            } else {
                ("Ready".to_string(), th().muted)
            };

            let ping_val = st
                .report
                .as_ref()
                .map(|r| format!("{:.1} ms (±{:.1})", r.ping_ms, r.jitter_ms))
                .unwrap_or_else(|| "—".into());
            let down_val = if st.running && st.live_mbps > 0.0 {
                format!("{:.1} Mbps", st.live_mbps)
            } else if let Some(r) = &st.report {
                format!("{:.1} Mbps", r.download_mbps)
            } else {
                "—".into()
            };
            let up_val = st
                .report
                .as_ref()
                .and_then(|r| r.upload_mbps)
                .map(|u| format!("{u:.1} Mbps"))
                .unwrap_or_else(|| "—".into());
            let rec_val = st
                .report
                .as_ref()
                .map(|r| format!("{} conns", r.recommended_connections))
                .unwrap_or_else(|| "—".into());
            let samples = if st.live_samples.len() >= 2 {
                st.live_samples.clone()
            } else {
                vec![0.0, 0.0]
            };

            div()
                .mx_4()
                .mb_2p5()
                .p_3()
                .rounded_xl()
                .bg(rgb(th().card))
                .border_1()
                .border_color(rgb(th().border))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::BOLD)
                                        .child("CONNECTION DIAGNOSTICS"),
                                )
                                .child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(rgb(tint(badge_col, 0.86)))
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(shade(badge_col, 0.62)))
                                        .child(badge_txt),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .when(st.running, |d| {
                                    d.child(btn(
                                        "st-cancel",
                                        "Cancel",
                                        th().elevated,
                                        th().text,
                                        th().danger_hover,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.cancel_speedtest();
                                            cx.notify();
                                        }),
                                    ))
                                })
                                .when(!st.running, |d| {
                                    d.child(btn(
                                        "st-run",
                                        "Run Test",
                                        th().accent,
                                        0xffffff,
                                        th().accent_hover,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.start_speedtest();
                                            cx.notify();
                                        }),
                                    ))
                                })
                                .child(btn(
                                    "st-close",
                                    "✕",
                                    th().elevated,
                                    th().muted,
                                    th().hover,
                                    cx.listener(|v, _e, _w, cx| {
                                        v.speedtest.open = false;
                                        cx.notify();
                                    }),
                                )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(div().text_xs().text_color(rgb(th().muted)).child("Ping / Jitter"))
                                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(ping_val)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(div().text_xs().text_color(rgb(th().muted)).child("Download"))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(th().accent))
                                        .child(down_val),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(div().text_xs().text_color(rgb(th().muted)).child("Upload"))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(GREEN))
                                        .child(up_val),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(div().text_xs().text_color(rgb(th().muted)).child("Optimal"))
                                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(rec_val)),
                        )
                        .child(sparkline_canvas(samples, 110., 34.)),
                )
                .when_some(st.error.clone(), |d, e| {
                    d.child(div().text_xs().text_color(rgb(RED)).child(e))
                })
        });

        let settings_page = self.settings_open.then(|| {
            let tab = self.settings_tab;
            let cfg = &self.config;
            let nav_tabs = [
                SettingsTab::Downloads,
                SettingsTab::Ninja,
                SettingsTab::Startup,
                SettingsTab::Security,
                SettingsTab::Appearance,
            ]
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let active = tab == t;
                div()
                    .id(("set-tab", i))
                    .relative()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .text_sm()
                    .when(active, |d| {
                        d.bg(rgb(th().accent_soft))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(th().accent))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top(px(6.))
                                    .bottom(px(6.))
                                    .w(px(3.))
                                    .rounded_full()
                                    .bg(rgb(th().accent)),
                            )
                    })
                    .when(!active, |d| {
                        d.text_color(rgb(th().muted))
                            .hover(|s| s.bg(rgb(th().elevated)).text_color(rgb(th().text)))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _e, _w, cx| {
                            v.settings_tab = t;
                            cx.notify();
                        }),
                    )
                    .child(t.label())
            });

            let card_box = || {
                div()
                    .rounded_xl()
                    .bg(rgb(th().card))
                    .border_1()
                    .border_color(rgb(th().border))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
            };

            let body = match tab {
                SettingsTab::Downloads => {
                    let dir_path = cfg.downloads.download_dir.clone();
                    let dir_str = dir_path.display().to_string();
                    let org = cfg.downloads.organize_by_category;
                    let conns = cfg.downloads.connections_per_download;
                    let conc = cfg.downloads.max_concurrent_downloads;
                    let rl = cfg.downloads.rate_limit_bps;

                    let conn_seg = div()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .p_0p5()
                        .rounded_lg()
                        .bg(rgb(th().elevated))
                        .border_1()
                        .border_color(rgb(th().border))
                        .children([4usize, 8, 12, 16].into_iter().map(|n| {
                            let on = conns == n;
                            btn(
                                ("cfg-conn", n as u64),
                                match n {
                                    4 => "4",
                                    8 => "8",
                                    12 => "12",
                                    _ => "16",
                                },
                                if on { th().accent } else { th().elevated },
                                if on { 0xffffff } else { th().text },
                                if on { th().accent_hover } else { th().hover },
                                cx.listener(move |v, _e, _w, cx| {
                                    v.config.downloads.connections_per_download = n;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            )
                        }));

                    let conc_seg = div()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .p_0p5()
                        .rounded_lg()
                        .bg(rgb(th().elevated))
                        .border_1()
                        .border_color(rgb(th().border))
                        .children([1usize, 2, 3, 5].into_iter().map(|n| {
                            let on = conc == n;
                            btn(
                                ("cfg-conc", n as u64),
                                match n {
                                    1 => "1",
                                    2 => "2",
                                    3 => "3",
                                    _ => "5",
                                },
                                if on { th().accent } else { th().elevated },
                                if on { 0xffffff } else { th().text },
                                if on { th().accent_hover } else { th().hover },
                                cx.listener(move |v, _e, _w, cx| {
                                    v.config.downloads.max_concurrent_downloads = n;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            )
                        }));

                    let rl_seg = div()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .p_0p5()
                        .rounded_lg()
                        .bg(rgb(th().elevated))
                        .border_1()
                        .border_color(rgb(th().border))
                        .children(
                            [
                                (None, "Unlimited"),
                                (Some(5 * 1024 * 1024), "5 MB/s"),
                                (Some(25 * 1024 * 1024), "25 MB/s"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, (val, lbl))| {
                                let on = rl == val;
                                btn(
                                    ("cfg-rl", i as u64),
                                    lbl,
                                    if on { th().accent } else { th().elevated },
                                    if on { 0xffffff } else { th().text },
                                    if on { th().accent_hover } else { th().hover },
                                    cx.listener(move |v, _e, _w, cx| {
                                        v.config.downloads.rate_limit_bps = val;
                                        let _ = v.config.save();
                                        cx.notify();
                                    }),
                                )
                            }),
                        );

                    let folder_chips = [
                        "Video/",
                        "Audio/",
                        "Documents/",
                        "Images/",
                        "Compressed/",
                        "Programs/",
                    ]
                    .into_iter()
                    .map(|c| {
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(rgb(th().elevated))
                            .border_1()
                            .border_color(rgb(th().border))
                            .text_xs()
                            .text_color(rgb(th().muted))
                            .child(c)
                    });

                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            card_box()
                                .child(setting_row(
                                    "Default Output Folder",
                                    dir_str,
                                    btn(
                                        "cfg-open-dir",
                                        "Open Folder",
                                        th().elevated,
                                        th().text,
                                        th().hover,
                                        move |_, _, _| {
                                            open_in_explorer(&dir_path);
                                        },
                                    ),
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "IDM Library Auto-Sort",
                                    "Automatically organize completed files into category subfolders.",
                                    toggle_switch(
                                        "cfg-org",
                                        org,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.config.downloads.organize_by_category =
                                                !v.config.downloads.organize_by_category;
                                            let _ = v.config.save();
                                            v.notice =
                                                Some((false, "saved library organization".into()));
                                            cx.notify();
                                        }),
                                    ),
                                ))
                                .child(
                                    div()
                                        .px_4()
                                        .pb_3()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .children(folder_chips),
                                ),
                        )
                        .child(
                            card_box()
                                .child(setting_row(
                                    "Parallel Connections per File",
                                    "Higher concurrency saturates high-speed links; 8 is ideal for most networks.",
                                    conn_seg,
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "Max Concurrent Downloads",
                                    "Maximum number of files transferring simultaneously in the queue.",
                                    conc_seg,
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "Global Speed Limit",
                                    "Cap total download bandwidth to keep browsing and calls responsive.",
                                    rl_seg,
                                )),
                        )
                }
                SettingsTab::Ninja => {
                    let mode = cfg.ninja.mode;
                    let action = cfg.ninja.action;
                    let watch_clip = cfg.ninja.watch_clipboard;
                    let watch_browser = cfg.ninja.watch_browser;
                    let domain_chips: Vec<_> = cfg
                        .ninja
                        .focused_domains
                        .iter()
                        .map(|d| {
                            div()
                                .px_2p5()
                                .py_0p5()
                                .rounded_full()
                                .bg(rgb(th().accent_soft))
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(th().accent))
                                .child(d.clone())
                        })
                        .collect();

                    let mode_seg = div()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .p_0p5()
                        .rounded_lg()
                        .bg(rgb(th().elevated))
                        .border_1()
                        .border_color(rgb(th().border))
                        .children(
                            [NinjaMode::Global, NinjaMode::Focused, NinjaMode::Off]
                                .into_iter()
                                .enumerate()
                                .map(|(i, m)| {
                                    let on = mode == m;
                                    btn(
                                        ("cfg-nm", i as u64),
                                        m.label(),
                                        if on { th().accent } else { th().elevated },
                                        if on { 0xffffff } else { th().text },
                                        if on { th().accent_hover } else { th().hover },
                                        cx.listener(move |v, _e, _w, cx| {
                                            v.config.ninja.mode = m;
                                            let _ = v.config.save();
                                            v.notice = Some((
                                                false,
                                                format!("Ninja mode -> {}", m.label()),
                                            ));
                                            cx.notify();
                                        }),
                                    )
                                }),
                        );

                    let act_seg = div()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .p_0p5()
                        .rounded_lg()
                        .bg(rgb(th().elevated))
                        .border_1()
                        .border_color(rgb(th().border))
                        .children(
                            [
                                (NinjaAction::GrabberQueue, "Grabber Queue"),
                                (NinjaAction::AutoDownload, "Auto-Download"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, (a, lbl))| {
                                let on = action == a;
                                btn(
                                    ("cfg-nact", i as u64),
                                    lbl,
                                    if on { th().accent } else { th().elevated },
                                    if on { 0xffffff } else { th().text },
                                    if on { th().accent_hover } else { th().hover },
                                    cx.listener(move |v, _e, _w, cx| {
                                        v.config.ninja.action = a;
                                        let _ = v.config.save();
                                        cx.notify();
                                    }),
                                )
                            }),
                        );

                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            card_box()
                                .child(setting_row(
                                    "Detection Scope",
                                    "Global catches all downloadable URLs; Focused only triggers on target domains/types.",
                                    mode_seg,
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "On URL Detection",
                                    "Queue in the 1-click Ninja Grabber bar or download silently right away.",
                                    act_seg,
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "Clipboard URL Sensor",
                                    "Detect downloadable links copied from any app (browser, chat, terminal).",
                                    toggle_switch(
                                        "cfg-nclip",
                                        watch_clip,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.config.ninja.watch_clipboard =
                                                !v.config.ninja.watch_clipboard;
                                            let _ = v.config.save();
                                            cx.notify();
                                        }),
                                    ),
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "Browser Extension Sensor",
                                    "Accept sniffed .m3u8/.mpd streams and intercepted downloads via od-nmh.",
                                    toggle_switch(
                                        "cfg-nbrow",
                                        watch_browser,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.config.ninja.watch_browser =
                                                !v.config.ninja.watch_browser;
                                            let _ = v.config.save();
                                            cx.notify();
                                        }),
                                    ),
                                )),
                        )
                        .child(
                            card_box().child(
                                div()
                                    .p_4()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Focused Target Domains"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(th().muted))
                                            .child("Edit via `od config set ninja.domains a.com,b.com` or the Web Settings modal."),
                                    )
                                    .child(div().flex().items_center().gap_1p5().children(domain_chips)),
                            ),
                        )
                }
                SettingsTab::Startup => {
                    let auto_on = cfg.startup.auto_start;
                    let resume_on = cfg.startup.auto_resume_on_start;
                    let cfg_file = AppConfig::default_path().display().to_string();
                    let running_others: Vec<DetectedDownloader> = self
                        .other_downloaders
                        .iter()
                        .filter(|d| d.running)
                        .cloned()
                        .collect();
                    let other_rows: Vec<_> = running_others
                        .iter()
                        .enumerate()
                        .map(|(idx, d)| {
                            let id_str = d.id.clone();
                            let name_str = d.name.clone();
                            let pid_text = format!("PID {:?}", d.pids);
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_4()
                                .py_2()
                                .border_t_1()
                                .border_color(rgb(th().border))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(6.))
                                                .h(px(6.))
                                                .rounded_full()
                                                .bg(rgb(AMBER)),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child(name_str),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(th().muted))
                                                .child(pid_text),
                                        ),
                                )
                                .child(btn(
                                    ("stop-other", idx as u64),
                                    "Stop",
                                    th().elevated,
                                    th().text,
                                    th().danger_hover,
                                    cx.listener(move |v, _e, _w, cx| {
                                        match od_core::stop_downloader(&id_str, false) {
                                            Ok(n) => {
                                                v.notice = Some((
                                                    false,
                                                    format!("stopped {n} process(es) for {id_str}"),
                                                ));
                                                v.other_downloaders = od_core::scan_downloaders();
                                            }
                                            Err(e) => {
                                                v.notice = Some((true, format!("stop failed: {e}")));
                                            }
                                        }
                                        cx.notify();
                                    }),
                                ))
                        })
                        .collect();

                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            card_box()
                                .child(setting_row(
                                    "Launch Automatically on OS Login",
                                    "Registers a silent user-level startup task (Task Scheduler / LaunchAgent / systemd).",
                                    toggle_switch(
                                        "cfg-autostart",
                                        auto_on,
                                        cx.listener(|v, _e, _w, cx| {
                                            let next = !v.config.startup.auto_start;
                                            let res = if next {
                                                std::env::current_exe()
                                                    .map_err(od_core::Error::Io)
                                                    .and_then(|exe| {
                                                        od_core::autostart::enable(
                                                            &od_core::autostart::AutoStartConfig::daemon(
                                                                exe,
                                                            ),
                                                        )
                                                    })
                                            } else {
                                                od_core::autostart::disable()
                                            };
                                            match res {
                                                Ok(_) => {
                                                    v.config.startup.auto_start = next;
                                                    let _ = v.config.save();
                                                    v.notice = Some((
                                                        false,
                                                        if next {
                                                            "enabled OS auto-start".into()
                                                        } else {
                                                            "disabled OS auto-start".into()
                                                        },
                                                    ));
                                                }
                                                Err(e) => {
                                                    v.notice = Some((true, format!("autostart error: {e}")));
                                                }
                                            }
                                            cx.notify();
                                        }),
                                    ),
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "Auto-Resume Interrupted Downloads",
                                    "Automatically continue unfinished .odj/.segments tasks on launch.",
                                    toggle_switch(
                                        "cfg-autoresume",
                                        resume_on,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.config.startup.auto_resume_on_start =
                                                !v.config.startup.auto_resume_on_start;
                                            let _ = v.config.save();
                                            cx.notify();
                                        }),
                                    ),
                                ))
                                .child(setting_divider())
                                .child(setting_row(
                                    "Configuration File",
                                    cfg_file,
                                    btn(
                                        "cfg-reset-all",
                                        "Reset Defaults",
                                        th().elevated,
                                        th().text,
                                        th().danger_hover,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.config = AppConfig::default();
                                            let _ = v.config.save();
                                            v.notice = Some((false, "restored default settings".into()));
                                            cx.notify();
                                        }),
                                    ),
                                )),
                        )
                        .child(
                            card_box()
                                .child(setting_row(
                                    "Other Download Managers",
                                    "Detect & gracefully stop competing downloaders (IDM, Gopeed, FDM, aria2, qBittorrent…).",
                                    btn(
                                        "cfg-scan-others",
                                        "Scan Running",
                                        th().accent_soft,
                                        th().accent,
                                        th().hover,
                                        cx.listener(|v, _e, _w, cx| {
                                            v.other_downloaders = od_core::scan_downloaders();
                                            let n = v.other_downloaders.iter().filter(|d| d.running).count();
                                            v.notice = Some((
                                                false,
                                                if n == 0 {
                                                    "no competing download managers running".into()
                                                } else {
                                                    format!("detected {n} running download manager(s)")
                                                },
                                            ));
                                            cx.notify();
                                        }),
                                    ),
                                ))
                                .when(other_rows.is_empty() && !self.other_downloaders.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .px_4()
                                            .pb_3()
                                            .text_xs()
                                            .text_color(rgb(GREEN))
                                            .child("✓ No competing download managers currently running."),
                                    )
                                })
                                .children(other_rows),
                        )
                }
                SettingsTab::Security => {
                    let tok = cfg.security.require_api_token;
                    let tls = cfg.security.verify_tls;
                    let san = cfg.security.sanitize_filenames;
                    card_box()
                        .child(setting_row(
                            "Local API & MCP Token Auth",
                            "Require x-od-token / Bearer authentication on 127.0.0.1:19799.",
                            toggle_switch(
                                "cfg-tok",
                                tok,
                                cx.listener(|v, _e, _w, cx| {
                                    v.config.security.require_api_token =
                                        !v.config.security.require_api_token;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            ),
                        ))
                        .child(setting_divider())
                        .child(setting_row(
                            "Verify HTTPS TLS Certificates",
                            "Validate server certificates via rustls-tls (recommended ON).",
                            toggle_switch(
                                "cfg-tls",
                                tls,
                                cx.listener(|v, _e, _w, cx| {
                                    v.config.security.verify_tls = !v.config.security.verify_tls;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            ),
                        ))
                        .child(setting_divider())
                        .child(setting_row(
                            "Filename Path-Traversal Guard",
                            "Strip ../ directory segments, illegal characters, and Windows reserved names.",
                            toggle_switch(
                                "cfg-san",
                                san,
                                cx.listener(|v, _e, _w, cx| {
                                    v.config.security.sanitize_filenames =
                                        !v.config.security.sanitize_filenames;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            ),
                        ))
                }
                SettingsTab::Appearance => {
                    let mode = cfg.appearance.theme;
                    let sp = cfg.appearance.show_sparkline;
                    let compact = cfg.appearance.compact_rows;
                    let theme_seg = div()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .p_0p5()
                        .rounded_lg()
                        .bg(rgb(th().elevated))
                        .border_1()
                        .border_color(rgb(th().border))
                        .children(
                            [
                                (ThemeMode::Dark, "Dark"),
                                (ThemeMode::Light, "Light"),
                                (ThemeMode::System, "System"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, (m, lbl))| {
                                let on = mode == m;
                                btn(
                                    ("cfg-theme", i as u64),
                                    lbl,
                                    if on { th().accent } else { th().elevated },
                                    if on { 0xffffff } else { th().text },
                                    if on { th().accent_hover } else { th().hover },
                                    cx.listener(move |v, _e, w, cx| {
                                        v.config.appearance.theme = m;
                                        let dark = match m {
                                            ThemeMode::Dark => true,
                                            ThemeMode::Light => false,
                                            ThemeMode::System => {
                                                matches!(w.appearance(), WindowAppearance::Dark)
                                            }
                                        };
                                        set_dark(dark);
                                        let _ = v.config.save();
                                        cx.notify();
                                    }),
                                )
                            }),
                        );
                    card_box()
                        .child(setting_row(
                            "Color Theme",
                            "Obsidian Cobalt (Dark), Porcelain Slate (Light), or follow OS appearance.",
                            theme_seg,
                        ))
                        .child(setting_divider())
                        .child(setting_row(
                            "Live Throughput Sparkline",
                            "Show the rolling 32-sample speed curve in the Telemetry Strip.",
                            toggle_switch(
                                "cfg-spark",
                                sp,
                                cx.listener(|v, _e, _w, cx| {
                                    v.config.appearance.show_sparkline =
                                        !v.config.appearance.show_sparkline;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            ),
                        ))
                        .child(setting_divider())
                        .child(setting_row(
                            "Compact Download Rows",
                            "Use tighter vertical spacing for high-density transfer queues.",
                            toggle_switch(
                                "cfg-compact",
                                compact,
                                cx.listener(|v, _e, _w, cx| {
                                    v.config.appearance.compact_rows =
                                        !v.config.appearance.compact_rows;
                                    let _ = v.config.save();
                                    cx.notify();
                                }),
                            ),
                        ))
                }
            };

            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_5()
                        .py_3p5()
                        .bg(rgb(th().sidebar))
                        .border_b_1()
                        .border_color(rgb(th().border))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(btn(
                                    "cfg-back",
                                    "← Back to Downloads",
                                    th().elevated,
                                    th().text,
                                    th().hover,
                                    cx.listener(|v, _e, _w, cx| {
                                        v.settings_open = false;
                                        cx.notify();
                                    }),
                                ))
                                .child(
                                    div()
                                        .text_base()
                                        .font_weight(FontWeight::BOLD)
                                        .child("Settings"),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(th().muted))
                                .child("Changes save automatically to config.json"),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .gap_4()
                        .p_5()
                        .child(
                            div()
                                .w(px(180.))
                                .flex_shrink_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(nav_tabs),
                        )
                        .child(
                            div()
                                .id("settings-scroll")
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_y_scroll()
                                .child(body),
                        ),
                )
        });

        let ninja_bar = (!self.ninja_queue.is_empty()).then(|| {
            let item = self.ninja_queue[0].clone();
            let extra_count = self.ninja_queue.len().saturating_sub(1);
            let url_to_dl = item.url.clone();
            let name_to_dl = item.filename.clone();
            div()
                .mx_4()
                .mb_2()
                .px_3()
                .py_2()
                .rounded_xl()
                .bg(rgb(th().accent_soft))
                .border_1()
                .border_color(rgb(th().accent))
                .flex()
                .items_center()
                .gap_2p5()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(rgb(th().accent))
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(0xffffff))
                        .child("NINJA"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(if extra_count > 0 {
                            format!(
                                "{} ({}) · +{extra_count} more",
                                item.filename, item.reason
                            )
                        } else {
                            format!("{} ({})", item.filename, item.reason)
                        }),
                )
                .child(btn(
                    "ninja-dl",
                    "Download",
                    th().accent,
                    0xffffff,
                    th().accent_hover,
                    cx.listener(move |v, _e, _w, cx| {
                        v.ninja_queue.remove(0);
                        v.start_download(url_to_dl.clone(), Some(name_to_dl.clone()), vec![]);
                        cx.notify();
                    }),
                ))
                .child(btn(
                    "ninja-dismiss",
                    "Dismiss",
                    th().elevated,
                    th().text,
                    th().hover,
                    cx.listener(|v, _e, _w, cx| {
                        if !v.ninja_queue.is_empty() {
                            v.ninja_queue.remove(0);
                        }
                        cx.notify();
                    }),
                ))
        });

        div()
            .flex()
            .size_full()
            .bg(rgb(th().bg))
            .text_color(rgb(th().text))
            // Windows' UI face; other platforms keep gpui's default.
            .when(cfg!(target_os = "windows"), |d| {
                d.font_family("Segoe UI")
            })
            .child(sidebar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    // Without this the widest row's min-content expands
                    // the whole column past the window edge.
                    .min_w(px(0.))
                    .when(self.settings_open, |d| {
                        d.when_some(settings_page, |d, page| d.child(page))
                    })
                    .when(!self.settings_open, |d| {
                        d.child(topbar)
                            .child(stats)
                            .when_some(speed_panel, |d, panel| d.child(panel))
                            .when_some(ninja_bar, |d, bar| d.child(bar))
                            .child(header)
                            .child(list)
                    })
                    .child(status),
            )
            .with_animation(
                "boot",
                Animation::new(Duration::from_millis(300)),
                |el, d| el.opacity(d),
            )
    }
}

fn row_view(row: &Row, phase: f32, cx: &Context<DownloadsView>) -> impl IntoElement + 'static {
    let id = row.spec.id;
    let frac = row.disp_frac;
    let size_text = match row.total {
        Some(t) if t > 0 => {
            format!("{} / {}", human_bytes(row.downloaded), human_bytes(t))
        }
        _ => human_bytes(row.downloaded),
    };
    let indeterminate = matches!(row.status, Status::Downloading) && row.total.is_none();
    let speed_text = if matches!(row.status, Status::Downloading) && row.speed > 0.0 {
        format!("{}/s", human_bytes(row.speed as u64))
    } else {
        "—".into()
    };
    // ETA only when all three inputs are known and positive.
    let meta_text = match (row.total, row.speed) {
        (Some(t), sp)
            if matches!(row.status, Status::Downloading)
                && t > row.downloaded
                && sp > 0.0 =>
        {
            format!(
                "{speed_text} · {size_text} · ~{} left",
                human_dur((t - row.downloaded) as f64 / sp)
            )
        }
        _ => format!("{speed_text} · {size_text}"),
    };
    let err = match &row.status {
        // Cap length: a long unbreakable token has a huge intrinsic
        // min-content width that can push the whole column off-screen.
        Status::Failed(e) => Some(e.chars().take(100).collect::<String>()
            + if e.chars().count() > 100 { "…" } else { "" }),
        _ => None,
    };
    let can_pause = matches!(row.status, Status::Downloading);
    let can_resume = matches!(row.status, Status::Paused | Status::Failed(_));
    let is_done = matches!(row.status, Status::Done);
    let dest = row.spec.dest.clone();
    let (icon, tile_bg, tile_fg) = tile_for(row.category());

    // Controls — pinned to the right, never squeezed by long filenames.
    let mut controls = div().flex().items_center().gap_1p5().flex_shrink_0();
    if can_pause {
        controls = controls.child(btn(
            ("pause", id),
            "Pause",
            th().elevated,
            th().text,
            th().hover,
            cx.listener(move |v, _e, _w, cx| {
                v.toggle(id);
                cx.notify();
            }),
        ));
    }
    if can_resume {
        controls = controls.child(btn(
            ("resume", id),
            "Resume",
            th().accent_soft,
            th().accent,
            th().hover,
            cx.listener(move |v, _e, _w, cx| {
                v.toggle(id);
                cx.notify();
            }),
        ));
    }
    if is_done {
        controls = controls.child(btn(
            ("open", id),
            "Reveal",
            th().elevated,
            th().text,
            th().hover,
            move |_, _, _| {
                open_in_explorer(&dest);
            },
        ));
    }
    controls = controls.child(btn(
        ("remove", id),
        "✕",
        th().elevated,
        th().muted,
        th().danger_hover,
        cx.listener(move |v, _e, _w, cx| {
            v.remove(id);
            cx.notify();
        }),
    ));

    // Progress bar — indeterminate slides a segment; determinate eases
    // toward the real fraction and carries a shimmer sweep while active.
    let bar = if indeterminate {
        let offset = (phase % 1.35) - 0.35;
        div().w_full().h_full().child(
            div()
                .h_full()
                .w(relative(0.35))
                .ml(relative(offset))
                .rounded_md()
                .bg(rgb(th().accent)),
        )
    } else {
        let mut fill = div()
            .h_full()
            .w(relative(frac))
            .rounded_full()
            .bg(rgb(status_color(&row.status)))
            .relative()
            .overflow_hidden();
        if matches!(row.status, Status::Downloading) {
            let mut shine = rgb(0xffffff);
            shine.a = 0.30;
            let p = phase % 1.0;
            fill = fill.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .w(relative(0.4))
                    .left(relative(-0.4 + p * 1.4))
                    .bg(shine),
            );
        }
        div().w_full().h_full().child(fill)
    };

    // Two-line row: name + status pill on top, progress bar + meta below.
    // min_w(0) lets the text column shrink so .truncate() can ellipsis
    // instead of the filename wrapping into a tall blob.
    let mut info = div()
        .flex()
        .items_center()
        .gap_3()
        .child(icon_tile(icon, tile_bg, tile_fg, 36.))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .child(SharedString::from(row.name.clone())),
                        )
                        .child(status_pill(&row.status, frac)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .h(px(6.))
                                .rounded_full()
                                .bg(rgb(th().track))
                                .child(bar),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(th().muted))
                                .whitespace_nowrap()
                                .flex_shrink_0()
                                .child(meta_text),
                        ),
                ),
        )
        .child(controls);
    if let Some(e) = err {
        info = div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            // min_w(0) zeroes the auto min-width the unbreakable error
            // token would otherwise propagate up the flex column chain.
            .min_w(px(0.))
            .child(info)
            // Same treatment as the filename: a flex row whose text child
            // is flex_1 + min_w(0) + truncate — that combination is what
            // actually bounds the unbreakable token's min-content width.
            .child(
                div().flex().pl(px(48.)).child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_xs()
                        .text_color(rgb(RED))
                        .truncate()
                        .child(e),
                ),
            );
    }

    // Entrance: rows fade in and drift up ~6px on first mount.
    div()
        .id(("row", id))
        .p_3()
        .rounded_xl()
        .bg(rgb(th().card))
        .border_1()
        .border_color(rgb(th().border))
        .hover(|s| s.border_color(rgb(th().border_hover)))
        .child(info)
        .with_animation(
            ("row-in", id),
            Animation::new(Duration::from_millis(240)).with_easing(|d| 1.0 - (1.0 - d).powi(3)),
            |el, d| el.opacity(d).mt(px((1.0 - d) * 6.0)),
        )
}

fn open_in_explorer(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
}

#[derive(serde::Deserialize)]
struct AddRequest {
    url: String,
    filename: Option<String>,
    cookie: Option<String>,
    referer: Option<String>,
}

/// `POST /add {"url": ..., "filename": ..., "cookie": ..., "referer": ...}`
/// — the native messaging host forwards browser downloads here. The
/// `x-od-token` header must match the shared token from [`od_core::auth_token`].
async fn serve_add_endpoint(
    listener: tokio::net::TcpListener,
    token: String,
    tx: mpsc::UnboundedSender<InboxItem>,
) {
    let app = axum::Router::new().route(
        "/add",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, axum::Json(req): axum::Json<AddRequest>| {
                let tx = tx.clone();
                let token = token.clone();
                async move {
                    let presented = headers
                        .get("x-od-token")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if token.is_empty() || presented != token {
                        return axum::http::StatusCode::UNAUTHORIZED;
                    }
                    if !(req.url.starts_with("http://") || req.url.starts_with("https://")) {
                        return axum::http::StatusCode::BAD_REQUEST;
                    }
                    let mut extra = Vec::new();
                    if let Some(c) = req.cookie.filter(|c| !c.is_empty()) {
                        extra.push(("cookie".to_string(), c));
                    }
                    if let Some(r) = req.referer.filter(|r| !r.is_empty()) {
                        extra.push(("referer".to_string(), r));
                    }
                    let _ = tx.send((req.url, req.filename, extra));
                    axum::http::StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let _ = axum::serve(listener, app).await;
}

fn main() {
    // The engine runs on tokio; GPUI runs its own executor. Bridge events
    // through the unbounded channel pumped inside the view.
    let rt = Arc::new(tokio::runtime::Runtime::new().expect("tokio runtime"));

    // Single instance: the /add port doubles as the instance lock.
    let listener = rt
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", od_core::APP_PORT)))
        .unwrap_or_else(|_| {
            eprintln!("Open Downloader is already running");
            std::process::exit(0);
        });
    let token = od_core::auth_token().unwrap_or_default();
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel::<InboxItem>();
    rt.spawn(serve_add_endpoint(listener, token, inbox_tx));

    let rt2 = rt.clone();
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        let client = reqwest::Client::new();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(760.0), px(480.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Open Downloader".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| {
                let (tx, rx) = mpsc::unbounded_channel();
                cx.new(|cx| DownloadsView::new(client, rt2.clone(), rx, tx, inbox_rx, cx))
            },
        )
        .unwrap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance — guards the theme palettes against
    /// unreadable text/bg pairs without needing eyeballs.
    fn contrast(fg: u32, bg: u32) -> f64 {
        let lum = |c: u32| {
            let ch = |s: u32| {
                let v = ((c >> s) & 0xff) as f64 / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
        };
        let (a, b) = (lum(fg), lum(bg));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn palettes_meet_contrast() {
        for theme in [LIGHT, DARK] {
            // Body text and small text vs the surfaces they sit on.
            assert!(contrast(theme.text, theme.card) >= 4.5, "text/card");
            assert!(contrast(theme.text, theme.bg) >= 4.5, "text/bg");
            assert!(contrast(theme.muted, theme.card) >= 4.0, "muted/card");
            assert!(contrast(theme.muted, theme.bg) >= 4.0, "muted/bg");
            // Accent text on cards.
            assert!(contrast(theme.accent, theme.card) >= 3.5, "accent/card");
            // Status labels sit on their own tints — same formula
            // status_pill uses.
            for c in [GREEN, AMBER, RED, theme.accent] {
                assert!(
                    contrast(shade(c, 0.62), tint(c, 0.86)) >= 4.0,
                    "status pill {c:#x}"
                );
            }
            // Category tiles: fg must read on its tile bg, both palettes.
            for tiles in [TILES.iter(), TILES_DARK.iter()] {
                for (_, _, bg, fg) in tiles {
                    assert!(contrast(*fg, *bg) >= 3.0, "tile {bg:#x}/{fg:#x}");
                }
            }
            // Progress track must be distinguishable from the card.
            assert!(contrast(theme.track, theme.card) >= 1.15, "track/card");
        }
        // On-accent foregrounds (brand icon, + Add) stay white.
        for theme in [LIGHT, DARK] {
            assert!(contrast(0xffffff, theme.accent) >= 3.5, "white/accent");
        }
    }

    #[test]
    fn theme_pref_parses() {
        assert_eq!(parse_theme_pref("dark\n"), Some(true));
        assert_eq!(parse_theme_pref("light"), Some(false));
        assert_eq!(parse_theme_pref(""), None);
        assert_eq!(parse_theme_pref("bogus"), None);
    }

    #[test]
    fn human_dur_scales() {
        assert_eq!(human_dur(42.0), "42s");
        assert_eq!(human_dur(125.0), "2m 05s");
        assert_eq!(human_dur(3725.0), "1h 02m");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0.0 B");
        assert_eq!(human_bytes(1023), "1023.0 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    /// The pause/resume button's code path: toggle cancels the live token,
    /// the engine's Cancelled finish flips the row to Paused, and toggling
    /// again relaunches with a fresh token (the journal resumes the data).
    #[gpui::test]
    fn pause_then_resume(cx: &mut gpui::TestAppContext) {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (_inbox_tx, inbox_rx) = mpsc::unbounded_channel::<InboxItem>();
        // Unroutable address: the engine task hangs instead of finishing
        // and racing the assertions.
        let view = cx.update(|cx| {
            cx.new(|cx| {
                DownloadsView::new(reqwest::Client::new(), rt, rx, tx, inbox_rx, cx)
            })
        });
        view.update(cx, |v, _cx| {
            v.start_download("http://10.255.255.1/never.bin".into(), None, vec![]);
            assert!(matches!(v.rows[0].status, Status::Downloading));

            let token = v.rows[0].cancel.clone().unwrap();
            v.toggle(0);
            assert!(token.is_cancelled());
            v.on_event(Event::Finished {
                id: 0,
                reason: FinishReason::Cancelled,
            });
            assert!(matches!(v.rows[0].status, Status::Paused));

            v.toggle(0);
            assert!(matches!(v.rows[0].status, Status::Downloading));
            let token2 = v.rows[0].cancel.clone().unwrap();
            assert!(!token2.is_cancelled());
        });
    }

    /// Regression: the eased bar must snap to full on completion —
    /// otherwise a finished row renders a partial (or for streams, empty)
    /// bar forever since the tick loop stops notifying.
    #[gpui::test]
    fn finish_snaps_progress_bar(cx: &mut gpui::TestAppContext) {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (_inbox_tx, inbox_rx) = mpsc::unbounded_channel::<InboxItem>();
        let view = cx.update(|cx| {
            cx.new(|cx| {
                DownloadsView::new(reqwest::Client::new(), rt, rx, tx, inbox_rx, cx)
            })
        });
        view.update(cx, |v, _cx| {
            v.start_download("http://10.255.255.1/movie.m3u8".into(), None, vec![]);
            // Stream: total stays None — this was the empty-bar bug.
            v.on_event(Event::Finished {
                id: 0,
                reason: FinishReason::Completed(od_core::DownloadOutcome {
                    path: std::path::PathBuf::from("movie.ts"),
                    resumed: false,
                }),
            });
            assert!(matches!(v.rows[0].status, Status::Done));
            assert_eq!(v.rows[0].disp_frac, 1.0);
        });
    }

    /// A download added while a filter/search is active must still appear.
    #[gpui::test]
    fn new_download_clears_filter(cx: &mut gpui::TestAppContext) {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (_inbox_tx, inbox_rx) = mpsc::unbounded_channel::<InboxItem>();
        let view = cx.update(|cx| {
            cx.new(|cx| {
                DownloadsView::new(reqwest::Client::new(), rt, rx, tx, inbox_rx, cx)
            })
        });
        view.update(cx, |v, _cx| {
            v.filter = Some(Category::Video);
            v.search_input = "zzz".into();
            v.start_download("http://10.255.255.1/b.zip".into(), None, vec![]);
            assert_eq!(v.visible_rows().count(), 1);
        });
    }

    /// The URL field validates before queueing.
    #[gpui::test]
    fn url_field_types_and_submits(cx: &mut gpui::TestAppContext) {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (_inbox_tx, inbox_rx) = mpsc::unbounded_channel::<InboxItem>();
        let view = cx.update(|cx| {
            cx.new(|cx| {
                DownloadsView::new(reqwest::Client::new(), rt, rx, tx, inbox_rx, cx)
            })
        });
        view.update(cx, |v, _cx| {
            v.url_input = "http://10.255.255.1/x.bin".into();
            v.submit_url();
            assert_eq!(v.rows.len(), 1);
            assert!(v.url_input.is_empty());

            v.url_input = "notaurl".into();
            v.submit_url();
            assert_eq!(v.rows.len(), 1);
            assert!(v.notice.is_some());
        });
    }

    /// Sidebar filtering: rows land under the same category the engine
    /// will sort them into.
    #[gpui::test]
    fn category_filter_hides_other_types(cx: &mut gpui::TestAppContext) {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let (_inbox_tx, inbox_rx) = mpsc::unbounded_channel::<InboxItem>();
        let view = cx.update(|cx| {
            cx.new(|cx| {
                DownloadsView::new(reqwest::Client::new(), rt, rx, tx, inbox_rx, cx)
            })
        });
        view.update(cx, |v, _cx| {
            v.start_download("http://10.255.255.1/a.mp4".into(), None, vec![]);
            v.start_download("http://10.255.255.1/b.zip".into(), None, vec![]);
            assert_eq!(v.visible_rows().count(), 2);

            v.filter = Some(Category::Video);
            assert_eq!(v.visible_rows().count(), 1);
            assert_eq!(v.visible_rows().next().unwrap().name, "a.mp4");

            v.search_input = "a.".into();
            assert_eq!(v.visible_rows().count(), 1);
            v.search_input = "zzz".into();
            assert_eq!(v.visible_rows().count(), 0);

            v.filter = None;
            v.search_input.clear();
            assert_eq!(v.visible_rows().count(), 2);
        });
    }
}
