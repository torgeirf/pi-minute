#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! π in one minute — GPU-rendered (wgpu: Direct3D 12 / Vulkan) live view of the computation.
//!
//! Usage: `pi-minute.exe [seconds]` (default 60).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke,
    TextureHandle, TextureOptions, Vec2,
};
use pi_minute::engine::{self, group, Phase, Shared, Snapshot};

const BG: Color32 = Color32::from_rgb(11, 13, 20);
const PANEL: Color32 = Color32::from_rgb(18, 21, 31);
const ACCENT: Color32 = Color32::from_rgb(255, 196, 61);
const ACCENT2: Color32 = Color32::from_rgb(86, 182, 255);
const DIM: Color32 = Color32::from_rgb(120, 128, 150);
const TEXT: Color32 = Color32::from_rgb(222, 226, 238);
const GOOD: Color32 = Color32::from_rgb(110, 220, 140);

/// One colour per decimal digit (perceptually spaced, "turbo"-like).
const DIGIT_COLORS: [Color32; 10] = [
    Color32::from_rgb(48, 18, 59),
    Color32::from_rgb(70, 98, 216),
    Color32::from_rgb(54, 170, 249),
    Color32::from_rgb(26, 228, 182),
    Color32::from_rgb(114, 254, 94),
    Color32::from_rgb(200, 239, 52),
    Color32::from_rgb(250, 186, 57),
    Color32::from_rgb(246, 107, 25),
    Color32::from_rgb(203, 42, 4),
    Color32::from_rgb(122, 4, 3),
];

fn main() -> eframe::Result {
    // `--seconds N`, `-s N` or a bare number. There is no console (windows subsystem), so a
    // bad argument is reported in the window instead and the default is used.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (secs, arg_error) = match pi_minute::parse_seconds(&args) {
        Ok((s, rest)) if rest.is_empty() => (s, None),
        Ok((s, rest)) => (s, Some(format!("Ignored unknown argument '{}'", rest[0]))),
        Err(e) => (pi_minute::DEFAULT_SECONDS, Some(format!("{e}; using {}", pi_minute::DEFAULT_SECONDS))),
    };

    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        setup.power_preference = eframe::wgpu::PowerPreference::HighPerformance;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("π in one minute")
            .with_inner_size([1480.0, 920.0])
            .with_min_inner_size([900.0, 600.0]),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "pi-minute",
        options,
        Box::new(move |cc| Ok(Box::new(PiApp::new(cc, secs, arg_error)))),
    )
}

struct PiApp {
    /// Time budget of the current/last run.
    budget: f64,
    /// Time budget for the next run, as edited in the header.
    next_budget: f64,
    engine: Arc<Shared>,
    gpu: String,
    seen_generation: u64,
    snapshot: Option<Arc<Snapshot>>,
    /// Live digit count target, kept monotonic within a run, and the eased value shown.
    live_shown: u64,
    live_display: f64,
    last_frame: f64,
    new_result_at: f64,
    texture: Option<TextureHandle>,
    texture_side: usize,
    scroll_to: Option<f32>,
    goto_digit: u64,
    status: Option<(String, f64)>,
}

impl PiApp {
    fn new(cc: &eframe::CreationContext<'_>, budget: f64, arg_error: Option<String>) -> Self {
        let gpu = cc
            .wgpu_render_state
            .as_ref()
            .map(|rs| {
                let info = rs.adapter.get_info();
                format!("{} ({:?})", info.name, info.backend)
            })
            .unwrap_or_else(|| "unknown".into());

        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.global_style_mut(|style| {
            style.visuals.panel_fill = PANEL;
            style.visuals.window_fill = PANEL;
            style.visuals.extreme_bg_color = BG;
            style.visuals.override_text_color = Some(TEXT);
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        });

        Self {
            budget,
            next_budget: budget,
            engine: engine::start(Duration::from_secs_f64(budget)),
            gpu,
            seen_generation: 0,
            snapshot: None,
            live_shown: 0,
            live_display: 0.0,
            last_frame: 0.0,
            new_result_at: -10.0,
            texture: None,
            texture_side: 0,
            scroll_to: None,
            goto_digit: 1,
            // Shown for a minute (see `header`), long enough to notice a bad argument.
            status: arg_error.map(|e| (e, 54.0)),
        }
    }

    fn restart(&mut self) {
        self.budget = self.next_budget;
        self.engine = engine::start(Duration::from_secs_f64(self.budget));
        self.seen_generation = 0;
        self.snapshot = None;
        self.live_shown = 0;
        self.live_display = 0.0;
        self.texture_side = 0;
        self.status = None;
    }

    /// Pulls a new result from the engine (if any) and refreshes the digit-map texture.
    fn poll(&mut self, ctx: &egui::Context, now: f64) {
        let generation = self.engine.generation.load(Ordering::Acquire);
        if generation == self.seen_generation {
            return;
        }
        self.seen_generation = generation;
        let Some(snap) = self.engine.latest.lock().unwrap().clone() else { return };
        self.new_result_at = now;

        // Digit map: smallest power-of-two square that holds every digit, up to 2048².
        let n = snap.digits.len();
        let mut side = 64usize;
        while side * side < n && side < 2048 {
            side *= 2;
        }
        let mut pixels = vec![BG; side * side];
        for (p, &d) in pixels.iter_mut().zip(snap.digits.iter()) {
            *p = DIGIT_COLORS[(d - b'0') as usize];
        }
        let image = ColorImage::new([side, side], pixels);
        let opts = if side <= 512 { TextureOptions::NEAREST } else { TextureOptions::LINEAR };
        match &mut self.texture {
            Some(t) => t.set(image, opts),
            None => self.texture = Some(ctx.load_texture("digit-map", image, opts)),
        }
        self.texture_side = side;
        self.snapshot = Some(snap);
    }

    fn save(&mut self, now: f64) {
        let Some(snap) = self.snapshot.clone() else { return };
        let path = std::env::current_dir()
            .unwrap_or_default()
            .join(format!("pi-{}-digits.txt", snap.digits.len()));
        self.status = Some((format!("Writing {}", path.display()), now));
        // Written in the background: hundreds of MB shouldn't stall the UI.
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path)?);
            f.write_all(b"3.")?;
            f.write_all(&snap.digits)?;
            f.flush()
        });
    }
}

impl eframe::App for PiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ui.input(|i| i.time);
        self.poll(&ctx, now);
        let done = self.engine.done.load(Ordering::Acquire);
        if !done || now - self.new_result_at < 2.0 {
            ctx.request_repaint(); // animate continuously while computing (vsync-paced)
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(18, 12)))
            .show(ui, |ui| self.header(ui, done, now));
        egui::Panel::left("stats")
            .exact_size(360.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::same(16)))
            .show(ui, |ui| self.stats(ui, done, now));
        egui::CentralPanel::default_margins()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(16)))
            .show(ui, |ui| self.central(ui, now));
    }
}

impl PiApp {
    fn header(&mut self, ui: &mut egui::Ui, done: bool, now: f64) {
        let elapsed = self.engine.elapsed();
        ui.horizontal(|ui| {
            ui.label(RichText::new("π").size(40.0).color(ACCENT).strong());
            ui.vertical(|ui| {
                let title = match self.budget {
                    60.0 => "in one minute".to_string(),
                    1.0 => "in one second".to_string(),
                    s => format!("in {} seconds", fmt_secs(s)),
                };
                ui.label(RichText::new(title).size(20.0).strong());
                ui.label(
                    RichText::new("Chudnovsky · parallel binary splitting · rendered on the GPU")
                        .size(12.0)
                        .color(DIM),
                );
            });
            ui.add_space(30.0);

            let left = (self.budget - elapsed).max(0.0);
            let (clock, color) = if done {
                (format!("done in {elapsed:.2} s"), GOOD)
            } else {
                (format!("{left:05.2} s left"), if left < 10.0 { ACCENT } else { TEXT })
            };
            ui.label(RichText::new(clock).size(30.0).monospace().color(color));

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let run = egui::Button::new(RichText::new("Run").strong());
                if ui
                    .add_enabled(done, run)
                    .on_disabled_hover_text("Available when the current run has finished")
                    .clicked()
                {
                    self.restart();
                }
                ui.add_enabled(
                    done,
                    egui::DragValue::new(&mut self.next_budget)
                        .range(pi_minute::MIN_SECONDS..=pi_minute::MAX_SECONDS)
                        .speed(1.0)
                        .max_decimals(1)
                        .suffix(" s"),
                )
                .on_hover_text("Time limit for the next run: drag, or click to type (1–3600 s)");
                ui.label(RichText::new("Time limit").color(DIM));
                ui.add_space(12.0);
                if ui
                    .add_enabled(self.snapshot.is_some(), egui::Button::new("Save digits"))
                    .clicked()
                {
                    self.save(now);
                }
                if let Some((msg, at)) = &self.status {
                    if now - at < 6.0 {
                        ui.label(RichText::new(msg).color(DIM));
                    }
                }
            });
        });
        ui.add_space(6.0);
        let frac = (elapsed / self.budget).clamp(0.0, 1.0) as f32;
        ui.add(
            egui::ProgressBar::new(frac)
                .desired_height(6.0)
                .fill(if done { GOOD } else { ACCENT }),
        );
    }

    fn stats(&mut self, ui: &mut egui::Ui, done: bool, now: f64) {
        let e = self.engine.clone();
        let digits = self.snapshot.as_ref().map_or(0, |s| s.digits.len() as u64);
        let final_pass = e.final_pass.load(Ordering::Acquire);

        // The live count follows the series as it's computed; decimal results arrive less
        // often, because converting hundreds of millions of digits takes seconds. The shown
        // value eases towards the target every frame so it never jumps.
        let live = if done { digits } else { e.live_digits().max(digits) };
        self.live_shown = self.live_shown.max(live);
        let dt = (now - self.last_frame).clamp(0.0, 0.25);
        self.last_frame = now;
        if done {
            self.live_display = digits as f64;
        } else {
            let target = self.live_shown as f64;
            self.live_display += (target - self.live_display) * (1.0 - (-dt / 0.12).exp());
            self.live_display = self.live_display.min(target);
        }
        let title = if done {
            "CORRECT DIGITS OF π"
        } else if final_pass {
            "DIGITS OF π COMPUTED ✓"
        } else {
            "DIGITS OF π COMPUTED"
        };
        ui.label(RichText::new(title).size(12.0).color(DIM));
        ui.label(
            RichText::new(group(self.live_display.round() as u64))
                .size(36.0)
                .monospace()
                .strong()
                .color(ACCENT),
        );
        if done {
            ui.label(
                RichText::new(format!("final result after {:.2} s", e.elapsed())).color(GOOD),
            );
        } else if let (true, Some((frac, _))) = (final_pass, e.final_progress()) {
            // Estimated from the final pass's predicted duration: the division and the
            // binary → decimal conversion don't report progress themselves.
            let total = digits_target(&e);
            ui.label(
                RichText::new(format!("≈ {}", group((frac as f64 * total as f64) as u64)))
                    .size(22.0)
                    .monospace()
                    .color(ACCENT2),
            );
            ui.label(RichText::new(format!("of {} converted to decimal (estimate)", group(total))).color(ACCENT2));
        } else if let Some(s) = &self.snapshot {
            ui.label(
                RichText::new(format!(
                    "{} converted to decimal and shown ({:.0} s ago)",
                    group(digits),
                    (e.elapsed() - s.at).max(0.0)
                ))
                .color(ACCENT2),
            );
        }
        ui.add_space(10.0);

        // --- what the engine is doing right now ---
        section(ui, "ENGINE");
        let series = e.series_phase.lock().unwrap().clone();
        let convert = e.convert_phase.lock().unwrap().clone();
        if !done {
            if matches!(series, Phase::Series { .. } | Phase::Merge) {
                ui.label(RichText::new(series.label()).size(12.5));
                let p = e.block_progress();
                ui.add(
                    egui::ProgressBar::new(p)
                        .desired_height(14.0)
                        .fill(ACCENT2)
                        .text(format!("series block {:.0} %", p * 100.0)),
                );
            } else if series != Phase::Idle {
                ui.label(RichText::new(series.label()).size(12.5));
            }
            if final_pass {
                ui.label(RichText::new(format!("Final pass (all cores): {}", convert.label())).size(12.5));
                if let Some((frac, left)) = e.final_progress() {
                    ui.add(egui::ProgressBar::new(frac).desired_height(10.0).fill(ACCENT));
                    ui.label(
                        RichText::new(format!(
                            "≈ {:.0} s left",
                            left.ceil()
                        ))
                        .size(12.5)
                        .color(ACCENT),
                    );
                }
            } else if convert != Phase::Idle {
                ui.label(RichText::new(format!("Preview: {}", convert.label())).size(12.5).color(DIM));
            }
        } else {
            ui.label(RichText::new("Finished").color(GOOD));
        }
        ui.add_space(8.0);

        egui::Grid::new("stats-grid").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).color(DIM));
                ui.label(RichText::new(v).monospace());
                ui.end_row();
            };
            let terms = e.series_terms.load(Ordering::Relaxed);
            row("Series terms", group(terms));
            if let Some(s) = &self.snapshot {
                row("Results shown", s.index.to_string());
                row("Digits / second", group((s.digits.len() as f64 / s.at.max(1e-6)) as u64));
                row("Last conversion", format!("{:.2} s", s.convert_secs));
                row(
                    "  √ / ÷ / decimal",
                    format!("{:.2} / {:.2} / {:.2} s", s.timing.prep, s.timing.divide, s.timing.decimal),
                );
                let check = if s.index <= 1 {
                    "—".to_string()
                } else {
                    format!("{} ✓", group(s.verified))
                };
                row("Matches previous", check);
            }
            row("CPU threads", format!("{} + 4 preview", e.threads));
        });
        ui.add(
            egui::Label::new(RichText::new(format!("GPU: {}", self.gpu)).size(12.0).color(DIM)).wrap(),
        );
        ui.add_space(12.0);

        section(ui, "DIGITS OVER TIME");
        self.chart(ui);
        ui.add_space(12.0);

        section(ui, "DIGIT FREQUENCY");
        if let Some(s) = &self.snapshot {
            histogram(ui, &s.histogram);
        }
    }

    fn chart(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 170.0), Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 6.0, BG);
        let plot = rect.shrink2(Vec2::new(12.0, 12.0)).with_min_x(rect.left() + 42.0);

        let shown = self.engine.history.lock().unwrap().clone();
        let series = self.engine.series_history.lock().unwrap().clone();
        let max_d = shown
            .iter()
            .chain(series.iter())
            .map(|&(_, d)| d)
            .max()
            .unwrap_or(10_000)
            .max(10_000) as f64;
        let (lo, hi) = (3.0f64, max_d.log10().ceil().max(4.0));
        let t_max = self.budget.max(self.engine.elapsed());
        let to_screen = |t: f64, d: u64| {
            let x = plot.left() + (t / t_max) as f32 * plot.width();
            let y = ((d.max(1) as f64).log10() - lo) / (hi - lo);
            Pos2::new(x, plot.bottom() - y.clamp(0.0, 1.0) as f32 * plot.height())
        };
        for decade in (lo as i32)..=(hi as i32) {
            let y = to_screen(0.0, 10u64.pow(decade as u32)).y;
            p.line_segment([Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)], Stroke::new(1.0, Color32::from_gray(35)));
            let label = match decade {
                3 => "1k",
                4 => "10k",
                5 => "100k",
                6 => "1M",
                7 => "10M",
                8 => "100M",
                9 => "1B",
                _ => "10B",
            };
            p.text(Pos2::new(rect.left() + 8.0, y), Align2::LEFT_CENTER, label, FontId::proportional(10.5), DIM);
        }
        // "now" marker
        let now_x = to_screen(self.engine.elapsed(), 1).x;
        p.line_segment([Pos2::new(now_x, plot.top()), Pos2::new(now_x, plot.bottom())], Stroke::new(1.0, Color32::from_gray(70)));

        let mut series_pts: Vec<Pos2> = Vec::new();
        for &(t, d) in &series {
            if let Some(last) = series_pts.last() {
                series_pts.push(Pos2::new(to_screen(t, 0).x, last.y));
            }
            series_pts.push(to_screen(t, d));
        }
        if series_pts.len() > 1 {
            p.add(egui::Shape::line(series_pts, Stroke::new(1.5, ACCENT2.gamma_multiply(0.7))));
        }
        let mut pts: Vec<Pos2> = Vec::new();
        for &(t, d) in &shown {
            if let Some(last) = pts.last() {
                pts.push(Pos2::new(to_screen(t, 0).x, last.y));
            }
            pts.push(to_screen(t, d));
        }
        if let Some(last) = pts.last().copied() {
            pts.push(Pos2::new(now_x.max(last.x), last.y));
        }
        if pts.len() > 1 {
            p.add(egui::Shape::line(pts.clone(), Stroke::new(2.5, ACCENT)));
        }
        for &(t, d) in &shown {
            p.circle_filled(to_screen(t, d), 2.5, ACCENT);
        }
        p.text(plot.left_top() + Vec2::new(4.0, 0.0), Align2::LEFT_TOP, "— digits shown", FontId::proportional(10.5), ACCENT);
        p.text(plot.left_top() + Vec2::new(4.0, 13.0), Align2::LEFT_TOP, "— series state", FontId::proportional(10.5), ACCENT2);
    }

    fn central(&mut self, ui: &mut egui::Ui, now: f64) {
        let Some(snap) = self.snapshot.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("warming up…").size(24.0).color(DIM));
            });
            return;
        };
        let digits = &snap.digits;

        // --- the frontier: the newest digits we know ---
        let flash = (1.0 - (now - self.new_result_at) / 1.2).clamp(0.0, 1.0) as f32;
        let color = lerp_color(TEXT, ACCENT, flash);
        let tail_len = 60.min(digits.len());
        let tail = std::str::from_utf8(&digits[digits.len() - tail_len..]).unwrap_or("");
        ui.label(
            RichText::new(format!(
                "LATEST DIGITS · positions {} – {}",
                group((digits.len() - tail_len + 1) as u64),
                group(digits.len() as u64)
            ))
            .size(12.0)
            .color(DIM),
        );
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("…{tail}")).size(26.0).monospace().color(color));
            let blink = ((now * 2.0).fract() < 0.5) && !self.engine.done.load(Ordering::Acquire);
            ui.label(RichText::new(if blink { "▌" } else { " " }).size(26.0).monospace().color(ACCENT));
        });
        ui.add_space(12.0);

        let avail = ui.available_size();
        let map_side = (avail.y - 24.0).min(avail.x * 0.45).max(100.0);
        ui.horizontal_top(|ui| {
            // --- digit map ---
            ui.vertical(|ui| {
                ui.set_width(map_side);
                let shown = (self.texture_side * self.texture_side).min(digits.len());
                ui.label(
                    RichText::new(format!(
                        "DIGIT MAP · first {} digits · {}×{} px, one per digit",
                        group(shown as u64),
                        self.texture_side,
                        self.texture_side
                    ))
                    .size(12.0)
                    .color(DIM),
                );
                if let Some(tex) = &self.texture {
                    ui.add(egui::Image::from_texture(tex).fit_to_exact_size(Vec2::splat(map_side)));
                }
                ui.horizontal(|ui| {
                    for (d, c) in DIGIT_COLORS.iter().enumerate() {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
                        ui.painter().rect_filled(r, 2.0, *c);
                        ui.label(RichText::new(d.to_string()).size(11.0).color(DIM));
                    }
                });
            });
            ui.add_space(16.0);

            // --- every digit, virtualised ---
            ui.vertical(|ui| self.digit_list(ui, digits));
        });
    }

    fn digit_list(&mut self, ui: &mut egui::Ui, digits: &[u8]) {
        let font = FontId::monospace(13.0);
        let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let label_w = char_w * 14.0;
        let groups = (((ui.available_width() - label_w - 16.0) / (char_w * 11.0)).floor() as usize).clamp(1, 10);
        let per_row = groups * 10;
        let rows = digits.len().div_ceil(per_row);
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let row_step = row_h + ui.spacing().item_spacing.y;

        ui.horizontal(|ui| {
            ui.label(RichText::new("ALL DIGITS").size(12.0).color(DIM));
            ui.add_space(12.0);
            if ui.small_button("Start").clicked() {
                self.scroll_to = Some(0.0);
            }
            if ui.small_button("End").clicked() {
                self.scroll_to = Some(rows as f32 * row_step);
            }
            ui.label(RichText::new("go to digit").size(12.0).color(DIM));
            let r = ui.add(
                egui::DragValue::new(&mut self.goto_digit)
                    .range(1..=digits.len().max(1) as u64)
                    .speed(1000.0),
            );
            if r.changed() || r.lost_focus() {
                let row = (self.goto_digit.saturating_sub(1) as usize) / per_row;
                self.scroll_to = Some(row as f32 * row_step);
            }
        });

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("digits");
        if let Some(y) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(y);
        }
        area.show_rows(ui, row_h, rows, |ui, range| {
            for row in range {
                let start = row * per_row;
                let end = (start + per_row).min(digits.len());
                let mut line = String::with_capacity(per_row + groups + 16);
                for (i, chunk) in digits[start..end].chunks(10).enumerate() {
                    if i > 0 {
                        line.push(' ');
                    }
                    line.push_str(std::str::from_utf8(chunk).unwrap_or(""));
                }
                ui.horizontal(|ui| {
                    let label = if row == 0 { "3.".to_string() } else { group(start as u64 + 1) };
                    ui.add_sized(
                        [label_w, row_h],
                        egui::Label::new(RichText::new(label).font(font.clone()).color(DIM)),
                    );
                    ui.label(RichText::new(line).font(font.clone()));
                });
            }
        });
    }
}

/// Uses Segoe UI (+ Segoe UI Symbol) from Windows, so glyphs like π, → and ✓ render. egui's
/// built-in fonts lack them. Monospace keeps egui's Hack font first.
fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};
    let mut fonts = FontDefinitions::default();
    let dir = std::path::Path::new(r"C:\Windows\Fonts");
    let mut added = Vec::new();
    for (name, file) in [("segoe", "segoeui.ttf"), ("segoe-symbol", "seguisym.ttf")] {
        if let Ok(bytes) = std::fs::read(dir.join(file)) {
            fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
            added.push(name.to_owned());
        }
    }
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        for (i, name) in added.iter().enumerate() {
            prop.insert(i, name.clone());
        }
    }
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        for (i, name) in added.iter().enumerate() {
            mono.insert(1 + i, name.clone());
        }
    }
    ctx.set_fonts(fonts);
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(12.0).color(DIM).strong());
    ui.separator();
}

fn histogram(ui: &mut egui::Ui, h: &[u64; 10]) {
    let total: u64 = h.iter().sum::<u64>().max(1);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 120.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, BG);
    let inner = rect.shrink2(Vec2::new(10.0, 18.0));
    let bar_w = inner.width() / 10.0;
    // Bars show deviation from the expected 10 %, scaled to make tiny differences visible.
    let max_dev = h
        .iter()
        .map(|&c| (c as f64 / total as f64 - 0.1).abs())
        .fold(1e-9, f64::max);
    let mid = inner.center().y;
    p.line_segment([Pos2::new(inner.left(), mid), Pos2::new(inner.right(), mid)], Stroke::new(1.0, Color32::from_gray(60)));
    for (d, &c) in h.iter().enumerate() {
        let frac = c as f64 / total as f64;
        let dev = (frac - 0.1) / max_dev; // -1..1
        let x0 = inner.left() + d as f32 * bar_w + 3.0;
        let x1 = x0 + bar_w - 6.0;
        let y = mid - dev as f32 * (inner.height() / 2.0 - 4.0);
        let r = Rect::from_min_max(Pos2::new(x0, y.min(mid)), Pos2::new(x1, y.max(mid) + 1.0));
        p.rect_filled(r, 2.0, DIGIT_COLORS[d]);
        p.text(Pos2::new((x0 + x1) / 2.0, rect.bottom() - 8.0), Align2::CENTER_CENTER, d.to_string(), FontId::monospace(11.0), DIM);
    }
    p.text(
        rect.left_top() + Vec2::new(8.0, 4.0),
        Align2::LEFT_TOP,
        format!("deviation from 10 %, max ±{:.4} %", max_dev * 100.0),
        FontId::proportional(10.5),
        DIM,
    );
}

/// Digits the series holds (what the final pass is converting).
fn digits_target(e: &Shared) -> u64 {
    engine::shown_digits(e.series_terms.load(Ordering::Acquire))
}

/// "45" for whole seconds, "2.5" otherwise.
fn fmt_secs(s: f64) -> String {
    if s.fract() == 0.0 { format!("{s:.0}") } else { format!("{s}") }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}
