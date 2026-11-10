//! The clip side of the interface: the player in the large view with its
//! timeline and trim handles, the color panel (histograms, levels, sliders) and
//! the export settings dialog.

use super::audio::Engine;
use super::export::{self, Encoder, MaxRes, Quality, Settings};
use super::grade::{Grade, Histogram, MID_RANGE, Tables};
use super::player::{Frame, Player};
use super::{Edit, VideoMeta};
use crate::{
    Action, App, Flag, GOLD, GREEN, STRIP_H, Strip, Thumb, film_strip, locate, mark_controls, nav_arrows, neighbours,
    paint_fit, strip_slots, At,
};
use eframe::egui::{self, Align, Color32, ColorImage, Key, Layout, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, pos2, vec2};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// What ffmpeg can do here, found out in the background at startup.
pub struct Caps {
    pub ffmpeg: bool,
    pub encoders: Vec<Encoder>,
}

pub fn detect_caps() -> Caps {
    let ffmpeg = super::ffmpeg_available();
    Caps { ffmpeg, encoders: if ffmpeg { export::detect() } else { Vec::new() } }
}

/// The frame on screen, kept ungraded so a slider move can regrade it while paused.
struct Shown {
    pts: f64,
    src: Arc<Vec<u8>>,
    size: [usize; 2],
    /// The grade the texture currently shows.
    grade: Option<Grade>,
    hist_in: Histogram,
    hist_out: Histogram,
}

/// Progress of an export's clips, shared with the export thread.
#[derive(Default)]
pub struct ClipProgress {
    /// Thousandths of the clip being encoded.
    pub clip: AtomicU32,
    pub label: Mutex<String>,
    pub cancel: AtomicBool,
}

pub struct VideoState {
    player: Option<(usize, Player)>,
    tex: Option<TextureHandle>,
    shown: Option<Shown>,
    tables: Option<(Grade, Tables)>,
    audio: Option<Engine>,
    audio_tried: bool,
    audio_epoch: u64,
    muted: bool,
    /// Dragging on the timeline: show cached keyframes while seeks catch up.
    scrubbing: bool,
    clipboard: Option<Grade>,
    show_original: bool,
    pub caps: Arc<Mutex<Option<Caps>>>,
    pub settings: Settings,
    pub dialog: bool,
    /// A clip whose grid thumbnail needs regrading once the slider is let go.
    pub thumb_dirty: Option<usize>,
}

impl VideoState {
    pub fn new(ctx: &egui::Context) -> Self {
        let caps = Arc::new(Mutex::new(None));
        let (c, ctx) = (caps.clone(), ctx.clone());
        std::thread::spawn(move || {
            *c.lock().unwrap() = Some(detect_caps());
            ctx.request_repaint();
        });
        VideoState {
            player: None,
            tex: None,
            shown: None,
            tables: None,
            audio: None,
            audio_tried: false,
            audio_epoch: 0,
            muted: false,
            scrubbing: false,
            clipboard: None,
            show_original: false,
            caps,
            settings: Settings::default(),
            dialog: false,
            thumb_dirty: None,
        }
    }

    /// Leave the player (back to the grid, another folder).
    pub fn close(&mut self) {
        if let Some(a) = &self.audio {
            a.stop();
        }
        self.player = None;
        self.shown = None;
    }

    pub fn ffmpeg_missing(&self) -> bool {
        self.caps.lock().unwrap().as_ref().is_some_and(|c| !c.ffmpeg)
    }
}

fn set_in(edit: &mut Edit, t: f64, duration: f64) -> bool {
    let (_, out) = edit.range(duration);
    let t = t.clamp(0.0, (out - 0.1).max(0.0));
    let changed = (edit.trim_in - t).abs() > 1e-6;
    edit.trim_in = if t < 0.01 { 0.0 } else { t };
    changed
}

fn set_out(edit: &mut Edit, t: f64, duration: f64) -> bool {
    let t = t.max(edit.trim_in + 0.1);
    let new = (t < duration - 0.05).then_some(t.min(duration));
    let changed = new != edit.trim_out;
    edit.trim_out = new;
    changed
}

enum Cmd {
    Toggle,
    Step(i32),
    Jump(f64),
    In,
    Out,
    Mute,
    Original,
}

impl App {
    /// Keep the player on the clip in the large view, play it on, and upload new
    /// or regraded frames.
    fn sync_video(&mut self, ctx: &egui::Context, idx: usize) {
        let p = &self.photos[idx];
        let Some(meta) = p.video.clone() else { return };
        let v = &mut self.video;
        if v.player.as_ref().is_none_or(|(i, _)| *i != idx) {
            v.close();
            v.scrubbing = false;
            let (start, _) = p.mark.edit.range(meta.duration);
            v.player = Some((idx, Player::new(p.path.clone(), meta, start)));
        }
        let (_, player) = v.player.as_mut().unwrap();
        let wait = player.tick();

        // Sound follows the player's clock.
        if player.epoch != v.audio_epoch {
            v.audio_epoch = player.epoch;
            match player.clock() {
                Some((at, from)) if player.meta.audio.is_some() => {
                    if v.audio.is_none() && !v.audio_tried {
                        v.audio_tried = true;
                        v.audio = Engine::new();
                    }
                    if let Some(a) = &v.audio {
                        a.muted.store(v.muted, Ordering::Relaxed);
                        a.play(player.path.clone(), from, at);
                    }
                }
                _ => {
                    if let Some(a) = &v.audio {
                        a.stop();
                    }
                }
            }
        }

        let new = player.take_new().map(|f| Frame { pts: f.pts, rgb: f.rgb.clone(), size: f.size });
        let new = new.or_else(|| {
            // Scrubbing ahead of the decoder: the nearest keyframe stands in.
            let t = player.position();
            let kf = (v.scrubbing && player.seeking()).then(|| player.keyframe_near(t)).flatten()?;
            v.shown.as_ref().is_none_or(|s| s.pts != kf.pts).then_some(kf)
        });
        if let Some(f) = new {
            v.shown = Some(Shown {
                pts: f.pts,
                hist_in: Histogram::of(&f.rgb, 3),
                hist_out: Histogram::of(&f.rgb, 3),
                src: f.rgb,
                size: f.size,
                grade: None,
            });
        }
        let grade = if v.show_original || meta_hdr(&self.photos[idx].video) { Grade::NONE } else { self.photos[idx].mark.edit.grade };
        if let Some(s) = &mut v.shown
            && s.grade != Some(grade)
        {
            if v.tables.as_ref().is_none_or(|(g, _)| *g != grade) {
                v.tables = Some((grade, grade.tables()));
            }
            let mut out = vec![0u8; s.src.len()];
            v.tables.as_ref().unwrap().1.apply(&s.src, &mut out);
            s.hist_out = Histogram::of(&out, 3);
            s.grade = Some(grade);
            let img = ColorImage::from_rgb(s.size, &out);
            match &mut v.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => v.tex = Some(ctx.load_texture("video", img, TextureOptions::LINEAR)),
            }
        }
        if let Some(w) = wait {
            ctx.request_repaint_after(w);
        }
    }

    /// Player keys, while a clip is in the large view.
    pub(crate) fn video_keys(&mut self, ctx: &egui::Context, idx: usize) {
        let mut cmds = Vec::new();
        ctx.input(|i| {
            if i.key_pressed(Key::Space) || i.key_pressed(Key::K) {
                cmds.push(Cmd::Toggle);
            }
            if i.key_pressed(Key::Period) {
                cmds.push(Cmd::Step(1));
            }
            if i.key_pressed(Key::Comma) {
                cmds.push(Cmd::Step(-1));
            }
            if i.key_pressed(Key::L) {
                cmds.push(Cmd::Jump(5.0));
            }
            if i.key_pressed(Key::J) {
                cmds.push(Cmd::Jump(-5.0));
            }
            if i.key_pressed(Key::OpenBracket) {
                cmds.push(Cmd::In);
            }
            if i.key_pressed(Key::CloseBracket) {
                cmds.push(Cmd::Out);
            }
            if i.key_pressed(Key::M) {
                cmds.push(Cmd::Mute);
            }
            if i.key_pressed(Key::Backslash) {
                cmds.push(Cmd::Original);
            }
        });
        for c in cmds {
            self.video_cmd(c, idx);
        }
    }

    fn video_cmd(&mut self, c: Cmd, idx: usize) {
        let v = &mut self.video;
        match c {
            Cmd::Mute => {
                v.muted = !v.muted;
                if let Some(a) = &v.audio {
                    a.muted.store(v.muted, Ordering::Relaxed);
                }
                return;
            }
            Cmd::Original => {
                v.show_original = !v.show_original;
                return;
            }
            _ => {}
        }
        let Some((_, player)) = v.player.as_mut() else { return };
        let (pos, dur) = (player.position(), player.duration());
        let edit = &mut self.photos[idx].mark.edit;
        let changed = match c {
            Cmd::Toggle => {
                // Playing from past the end of the kept part starts at its beginning.
                let (tin, tout) = edit.range(dur);
                if !player.playing() && edit.trimmed() && (pos >= tout - 0.05 || pos < tin) {
                    player.seek(tin);
                }
                player.toggle();
                false
            }
            Cmd::Step(n) => {
                player.step(n);
                false
            }
            Cmd::Jump(d) => {
                player.seek(pos + d);
                false
            }
            Cmd::In => set_in(edit, pos, dur),
            Cmd::Out => set_out(edit, pos, dur),
            Cmd::Mute | Cmd::Original => false,
        };
        if changed {
            self.marks_dirty = true;
        }
    }

    pub(crate) fn video_viewer(&mut self, ui: &mut egui::Ui, idx: usize, vis: &[usize]) -> Vec<Action> {
        let mut actions = Vec::new();
        self.sync_video(ui.ctx(), idx);
        let at = locate(vis, idx, self.anchor);
        let strip = match at {
            Some(at) if self.show_strip => strip_slots(vis, idx, at, ui.available_width()),
            _ => (Vec::new(), 0.0),
        };
        let gone = matches!(at, Some(At::Gone(_)));
        let (prev, next) = neighbours(vis, at);

        let p = &self.photos[idx];
        let mut back = false;
        ui.horizontal_wrapped(|ui| {
            back = ui.button("⬅ Grid (Esc)").clicked();
            ui.separator();
            ui.label(RichText::new(&p.rel).strong());
            ui.label(RichText::new(&p.date).weak());
            ui.separator();
            ui.horizontal(|ui| mark_controls(ui, p, idx, 20.0, &mut actions));
            ui.separator();
            if let Some(m) = &p.video {
                ui.label(RichText::new(m.summary()).weak());
            }
            if self.video.player.as_ref().is_some_and(|(_, pl)| pl.seeking()) {
                ui.spinner();
            }
            ui.toggle_value(&mut self.show_strip, "🎞 Strip").on_hover_text("Show neighbouring files below (S)");
            if self.video.show_original {
                ui.label(RichText::new("showing original (\\)").color(GOLD));
            }
        });
        if back {
            self.viewing = false;
        }
        ui.add_space(4.0);

        const TRANSPORT_H: f32 = 64.0;
        let mut area = ui.available_size();
        area.y = (area.y - TRANSPORT_H - if strip.0.is_empty() { 0.0 } else { STRIP_H + 4.0 }).max(0.0);
        let (rect, resp) = ui.allocate_exact_size(area, Sense::click());
        ui.painter().rect_filled(rect, 0.0, Color32::from_gray(8));
        let shown_tex = self.video.tex.as_ref().filter(|_| self.video.shown.is_some());
        match (&self.photos[idx].video, shown_tex) {
            (_, Some(tex)) => {
                let tint = if self.photos[idx].mark.flag == Flag::Rejected { Color32::from_gray(90) } else { Color32::WHITE };
                paint_fit(ui, tex, rect, tint);
            }
            (None, _) if matches!(self.photos[idx].thumb, Thumb::Failed) || self.video.ffmpeg_missing() => {
                let msg = if self.video.ffmpeg_missing() {
                    "Playing clips needs ffmpeg.\nInstall it and make sure `ffmpeg` and `ffprobe` are on your PATH\n(or point SLOPSHOT_FFMPEG at ffmpeg), then restart slopshot."
                } else {
                    "This clip couldn't be read."
                };
                ui.put(rect, egui::Label::new(RichText::new(msg).weak()));
            }
            _ => {
                ui.put(rect, egui::Spinner::new().size(32.0));
            }
        }
        if resp.clicked() {
            self.video_cmd(Cmd::Toggle, idx);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        nav_arrows(ui, rect, prev, next, &mut actions);

        self.transport(ui, idx);

        if !strip.0.is_empty() {
            ui.add_space(4.0);
            let (strip_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), STRIP_H), Sense::hover());
            let strip = Strip { slots: strip.0, frame_w: strip.1, current: idx, gone };
            film_strip(ui, &self.photos, &strip, strip_rect, &mut actions);
        }
        actions
    }

    fn transport(&mut self, ui: &mut egui::Ui, idx: usize) {
        let mut cmd = None;
        let Some((_, player)) = self.video.player.as_mut() else {
            ui.add_space(64.0);
            return;
        };
        let (pos, dur) = (player.position(), player.duration());
        let edit = &mut self.photos[idx].mark.edit;
        let mut changed = false;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let play = if player.playing() { "⏸" } else { "▶" };
            if ui.add(egui::Button::new(RichText::new(play).size(16.0)).min_size(vec2(34.0, 0.0))).on_hover_text("Play / pause (Space)").clicked() {
                cmd = Some(Cmd::Toggle);
            }
            if ui.button("⏮").on_hover_text("Go to the start of the kept part").clicked() {
                player.seek(edit.range(dur).0);
            }
            if ui.button("◀").on_hover_text("Previous frame (,)").clicked() {
                cmd = Some(Cmd::Step(-1));
            }
            if ui.button("▶").on_hover_text("Next frame (.)").clicked() {
                cmd = Some(Cmd::Step(1));
            }
            ui.label(RichText::new(format!("{} / {}", super::timecode(pos), super::timecode(dur))).monospace());
            ui.separator();
            if ui.button("[ Set start").on_hover_text("Trim: keep from here ([)").clicked() {
                changed |= set_in(edit, pos, dur);
            }
            if ui.button("Set end ]").on_hover_text("Trim: keep up to here (])").clicked() {
                changed |= set_out(edit, pos, dur);
            }
            if edit.trimmed() {
                let (a, b) = edit.range(dur);
                ui.label(RichText::new(format!("✂ keeping {} – {} ({:.1} s)", super::timecode(a), super::timecode(b), b - a)).color(GOLD));
                if ui.small_button("Clear").on_hover_text("Keep the whole clip").clicked() {
                    edit.trim_in = 0.0;
                    edit.trim_out = None;
                    changed = true;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if player.meta.audio.is_some() {
                    let icon = if self.video.muted { "🔇" } else { "🔊" };
                    if ui.button(icon).on_hover_text("Mute (M)").clicked() {
                        cmd = Some(Cmd::Mute);
                    }
                } else {
                    ui.label(RichText::new("no sound").weak().small());
                }
            });
        });
        ui.add_space(2.0);
        changed |= timeline(ui, player, edit, &mut self.video.scrubbing);
        if changed {
            self.marks_dirty = true;
            self.video.thumb_dirty = Some(idx);
        }
        if let Some(c) = cmd {
            self.video_cmd(c, idx);
        }
    }

    /// The right-hand panel while a clip is in the large view.
    pub(crate) fn color_panel(&mut self, ctx: &egui::Context, idx: usize) {
        egui::SidePanel::right("color").resizable(false).exact_width(280.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(6.0);
                let Some(meta) = self.photos[idx].video.clone() else {
                    ui.label(RichText::new("Reading clip…").weak());
                    return;
                };
                ui.heading("Color");
                if meta.hdr {
                    ui.label(RichText::new("HDR clip (HLG/PQ): color editing works on SDR footage only. Trim and compression still apply, and the export stays HDR.").weak());
                }
                ui.add_enabled_ui(!meta.hdr, |ui| self.color_controls(ui, idx));
                ui.add_space(8.0);
                egui::CollapsingHeader::new("Clip info").default_open(true).show(ui, |ui| {
                    clip_info(ui, &self.photos[idx].path, &self.photos[idx].rel, &self.photos[idx].date, &meta);
                });
            });
        });
    }

    fn color_controls(&mut self, ui: &mut egui::Ui, idx: usize) {
        let mut g = self.photos[idx].mark.edit.grade;
        let before = g;
        let (hist_in, hist_out) = match &self.video.shown {
            Some(s) => (Some(s.hist_in.clone()), Some(s.hist_out.clone())),
            None => (None, None),
        };
        ui.add_space(4.0);
        ui.label(RichText::new("Levels — drag the handles").small().weak());
        levels(ui, hist_in.as_ref(), &mut g);
        ui.add_space(6.0);
        ui.label(RichText::new("Result").small().weak());
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 70.0), Sense::hover());
        paint_histogram(ui, r, hist_out.as_ref());
        ui.add_space(6.0);

        let reset = |ui: &mut egui::Ui, v: &mut f32, range: std::ops::RangeInclusive<f32>, label: &str, default: f32, tip: &str| {
            let r = ui.add(egui::Slider::new(v, range).text(label).fixed_decimals(2)).on_hover_text(format!("{tip}\nDouble-click to reset"));
            if r.double_clicked() {
                *v = default;
            }
        };
        ui.spacing_mut().slider_width = 125.0;
        reset(ui, &mut g.exposure, -3.0..=3.0, "Exposure", 0.0, "Brightness in stops");
        reset(ui, &mut g.contrast, -1.0..=1.0, "Contrast", 0.0, "S-curve; negative flattens");
        reset(ui, &mut g.saturation, 0.0..=2.0, "Saturation", 1.0, "0 is black and white");
        reset(ui, &mut g.temperature, -1.0..=1.0, "Temperature", 0.0, "Cooler ← → warmer");
        reset(ui, &mut g.tint, -1.0..=1.0, "Tint", 0.0, "Greener ← → more magenta");
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(hist_in.is_some(), egui::Button::new("Auto levels")).on_hover_text("Stretch levels to this frame's darkest and brightest").clicked() {
                g = g.auto_levels(hist_in.as_ref().unwrap());
            }
            if ui.add_enabled(!g.is_identity(), egui::Button::new("Reset")).clicked() {
                g = Grade::NONE;
            }
            if ui.button("Copy").on_hover_text("Copy this clip's color").clicked() {
                self.video.clipboard = Some(g);
            }
            if ui.add_enabled(self.video.clipboard.is_some(), egui::Button::new("Paste")).on_hover_text("Use the copied color on this clip").clicked() {
                g = self.video.clipboard.unwrap();
            }
        });
        ui.checkbox(&mut self.video.show_original, "Show original (\\)");
        if g != before {
            self.photos[idx].mark.edit.grade = g;
            self.marks_dirty = true;
            self.video.thumb_dirty = Some(idx);
        }
    }

    /// Asked when the export includes clips.
    pub(crate) fn export_dialog(&mut self, ctx: &egui::Context) {
        if !self.video.dialog {
            return;
        }
        let picked = |video: bool| self.photos.iter().filter(|p| p.mark.flag == Flag::Picked && p.is_video == video).count();
        let (photos, clips) = (picked(false), picked(true));
        let (ffmpeg, encoders) = match &*self.video.caps.lock().unwrap() {
            Some(c) => (Some(c.ffmpeg), c.encoders.clone()),
            None => (None, Vec::new()),
        };
        let s = &mut self.video.settings;
        if !encoders.is_empty() && !encoders.contains(&s.encoder) {
            s.encoder = encoders[0];
        }
        let mut go = None;
        let modal = egui::Modal::new(egui::Id::new("export_clips")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Export picked files");
            ui.add_space(4.0);
            if photos > 0 {
                ui.label(format!("{photos} photo(s) are copied as they are."));
            }
            ui.label(format!("{clips} clip(s):"));
            ui.add_space(4.0);
            ui.radio_value(&mut s.reencode, true, "Compress — apply trim and color, smaller files");
            ui.radio_value(&mut s.reencode, false, "Copy without re-encoding — fast, full size");
            ui.add_space(6.0);
            if s.reencode {
                egui::Grid::new("export_settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Encoder");
                    if ffmpeg.is_none() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("checking what this machine can encode…").weak());
                        });
                    } else {
                        egui::ComboBox::from_id_salt("encoder").width(240.0).selected_text(s.encoder.label()).show_ui(ui, |ui| {
                            for e in &encoders {
                                ui.selectable_value(&mut s.encoder, *e, e.label());
                            }
                        });
                    }
                    ui.end_row();
                    ui.label("Size");
                    egui::ComboBox::from_id_salt("size").width(240.0).selected_text(s.max_res.label()).show_ui(ui, |ui| {
                        for r in MaxRes::ALL {
                            ui.selectable_value(&mut s.max_res, r, r.label());
                        }
                    });
                    ui.end_row();
                    ui.label("Quality");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut s.quality, Quality::High, "High");
                        ui.selectable_value(&mut s.quality, Quality::Balanced, "Balanced");
                        ui.selectable_value(&mut s.quality, Quality::Small, "Small");
                    });
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Clips are never upscaled. Capture date, time and GPS location are kept, so Immich files them in the right place.").small().weak());
            } else {
                ui.label(RichText::new("Trims cut at the keyframe at or before the start (up to about a second early). Color edits aren't applied.").small().weak());
            }
            if ffmpeg == Some(false) {
                ui.add_space(4.0);
                ui.colored_label(crate::RED, "ffmpeg wasn't found, so clips can't be exported. Install it and restart slopshot.");
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ready = ffmpeg == Some(true) && (!s.reencode || !encoders.is_empty());
                if ui.add_enabled(ready, egui::Button::new("Choose folder and export…")).clicked() {
                    go = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    go = Some(false);
                }
            });
        });
        if modal.should_close() && go.is_none() {
            go = Some(false);
        }
        match go {
            Some(true) => {
                self.video.dialog = false;
                let settings = self.video.settings;
                self.export_to(ctx, Some(settings));
            }
            Some(false) => self.video.dialog = false,
            None => {}
        }
    }
}

fn meta_hdr(m: &Option<VideoMeta>) -> bool {
    m.as_ref().is_some_and(|m| m.hdr)
}

#[derive(Clone, Copy, PartialEq)]
enum Grab {
    In,
    Out,
    Head,
}

/// Timeline: the kept part highlighted between two trim handles, the playhead,
/// click or drag to scrub, drag a handle to trim. Returns whether the trim changed.
fn timeline(ui: &mut egui::Ui, player: &mut Player, edit: &mut Edit, scrubbing: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click_and_drag());
    let dur = player.duration().max(1e-3);
    let x_of = |t: f64| rect.left() + (t / dur).clamp(0.0, 1.0) as f32 * rect.width();
    let t_of = |x: f32| ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * dur;
    let (tin, tout) = edit.range(dur);
    let grab_id = resp.id.with("grab");
    let mut changed = false;

    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
    {
        // The handles sit at the ends until moved; drag one in to trim.
        let (d_in, d_out) = ((x_of(tin) - p.x).abs(), (x_of(tout) - p.x).abs());
        let grab = if d_in.min(d_out) > 7.0 {
            Grab::Head
        } else if d_in < d_out || (d_in == d_out && p.x < x_of(tin)) {
            Grab::In
        } else {
            Grab::Out
        };
        ui.memory_mut(|m| m.data.insert_temp(grab_id, grab));
    }
    let pointer = resp.interact_pointer_pos();
    if let (true, Some(p)) = (resp.dragged() || resp.clicked(), pointer) {
        let t = t_of(p.x);
        let grab = if resp.clicked() { Grab::Head } else { ui.memory(|m| m.data.get_temp(grab_id)).unwrap_or(Grab::Head) };
        match grab {
            Grab::In => changed |= set_in(edit, t, dur),
            Grab::Out => changed |= set_out(edit, t, dur),
            Grab::Head => {}
        }
        let to = match grab {
            Grab::In => edit.trim_in,
            Grab::Out => edit.range(dur).1.min(dur - player.meta.frame_time()),
            Grab::Head => t,
        };
        if (to - player.position()).abs() > player.meta.frame_time() * 0.5 {
            player.seek(to);
        }
        *scrubbing = resp.dragged();
    }
    if resp.drag_stopped() {
        *scrubbing = false;
    }

    let painter = ui.painter_at(rect.expand(2.0));
    let bar = rect.shrink2(vec2(0.0, 7.0));
    painter.rect_filled(bar, 3.0, Color32::from_gray(45));
    let (tin, tout) = edit.range(dur);
    let kept = Rect::from_x_y_ranges(x_of(tin)..=x_of(tout), bar.y_range());
    painter.rect_filled(kept, 3.0, if edit.trimmed() { GOLD.gamma_multiply(0.55) } else { Color32::from_gray(85) });
    for t in [tin, tout] {
        let h = Rect::from_center_size(pos2(x_of(t), rect.center().y), vec2(5.0, rect.height()));
        painter.rect_filled(h, 2.0, if edit.trimmed() { GOLD } else { Color32::from_gray(150) });
    }
    let x = x_of(player.position());
    painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0, Color32::WHITE));
    if let Some(h) = resp.hover_pos() {
        resp.clone().on_hover_text_at_pointer(super::timecode(t_of(h.x)));
        if (x_of(tin) - h.x).abs() <= 7.0 || (x_of(tout) - h.x).abs() <= 7.0 {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }
    changed
}

/// RGB histogram, channels overlaid; scaled to the tallest bin away from the ends
/// (clipped blacks/whites would otherwise flatten everything else).
fn paint_histogram(ui: &egui::Ui, rect: Rect, h: Option<&Histogram>) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, Color32::from_gray(18));
    let Some(h) = h else { return };
    let peak = h.rgb.iter().flat_map(|c| c[2..254].iter()).copied().max().unwrap_or(1).max(1) as f32;
    let colors = [Color32::from_rgba_unmultiplied(255, 60, 60, 110), Color32::from_rgba_unmultiplied(60, 255, 60, 110), Color32::from_rgba_unmultiplied(70, 110, 255, 130)];
    let w = rect.width() / 256.0;
    for (c, col) in h.rgb.iter().zip(colors) {
        for (i, &n) in c.iter().enumerate() {
            let hgt = (n as f32 / peak).min(1.0) * (rect.height() - 2.0);
            if hgt < 0.5 {
                continue;
            }
            let x = rect.left() + i as f32 * w;
            painter.rect_filled(Rect::from_min_max(pos2(x, rect.bottom() - hgt), pos2(x + w.max(1.0), rect.bottom())), 0.0, col);
        }
    }
}

/// Source histogram with Photoshop-style levels handles under it: black point,
/// midtones and white point.
fn levels(ui: &mut egui::Ui, hist: Option<&Histogram>, g: &mut Grade) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 70.0), Sense::hover());
    paint_histogram(ui, rect, hist);
    let painter = ui.painter_at(rect);
    let x_of = |v: f32| rect.left() + v.clamp(0.0, 1.0) * rect.width();
    for (v, c) in [(g.black, Color32::from_gray(140)), (g.white, Color32::from_gray(140))] {
        painter.line_segment([pos2(x_of(v), rect.top()), pos2(x_of(v), rect.bottom())], Stroke::new(1.0, c));
    }

    let (track, _) = ui.allocate_exact_size(vec2(rect.width(), 16.0), Sense::hover());
    // Where the midtone handle sits: the input that comes out at 50% grey.
    let mid_pos = |g: &Grade| g.black + (g.white - g.black) * 0.5f32.powf(g.mid);
    let handles = [(0, g.black, Color32::BLACK), (1, mid_pos(g), Color32::from_gray(128)), (2, g.white, Color32::WHITE)];
    for (k, v, fill) in handles {
        let x = x_of(v);
        let tri = vec![pos2(x, track.top()), pos2(x - 7.0, track.bottom()), pos2(x + 7.0, track.bottom())];
        let hit = Rect::from_center_size(pos2(x, track.center().y), vec2(16.0, 18.0));
        let resp = ui.interact(hit, ui.id().with(("levels", k)), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        let stroke = if resp.hovered() || resp.dragged() { Stroke::new(1.5, GREEN) } else { Stroke::new(1.0, Color32::from_gray(170)) };
        ui.painter().add(egui::Shape::convex_polygon(tri, fill, stroke));
        if let (true, Some(p)) = (resp.dragged(), resp.interact_pointer_pos()) {
            let v = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            match k {
                0 => g.black = v.min(g.white - 0.05),
                2 => g.white = v.max(g.black + 0.05),
                _ => {
                    let f = ((v - g.black) / (g.white - g.black)).clamp(0.01, 0.99);
                    g.mid = (f.ln() / 0.5f32.ln()).clamp(MID_RANGE.0, MID_RANGE.1);
                }
            }
        }
        if resp.double_clicked() {
            match k {
                0 => g.black = 0.0,
                2 => g.white = 1.0,
                _ => g.mid = 1.0,
            }
        }
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{:.0}", g.black * 255.0)).small().monospace());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(format!("{:.0}", g.white * 255.0)).small().monospace());
            ui.with_layout(Layout::top_down(Align::Center), |ui| ui.label(RichText::new(format!("{:.2}", g.mid)).small().monospace()));
        });
    });
}

pub(crate) fn clip_info(ui: &mut egui::Ui, path: &Path, rel: &str, date: &str, m: &VideoMeta) {
    let file_size = std::fs::metadata(path).ok().map(|f| format!("{:.1} MB", f.len() as f64 / 1e6));
    let rows = [
        ("File", Some(rel.to_string())),
        ("Recorded", Some(date.to_string())),
        ("Length", Some(super::timecode(m.duration))),
        ("Resolution", Some(format!("{} × {}", m.width, m.height))),
        ("Frame rate", Some(m.fps_label())),
        ("Video", Some(format!("{} · {}-bit{}", m.codec, m.bit_depth, if m.hdr { " · HDR" } else { "" }))),
        ("Bitrate", m.bitrate.map(|b| format!("{:.0} Mbit/s", b as f64 / 1e6))),
        ("Audio", Some(m.audio.clone().unwrap_or_else(|| "none".into()))),
        ("Location", m.location.clone()),
        ("File size", file_size),
    ];
    egui::Grid::new("clip_info").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        for (k, v) in rows {
            ui.label(RichText::new(k).weak());
            ui.add(egui::Label::new(v.unwrap_or_else(|| "—".into())).wrap());
            ui.end_row();
        }
    });
}

/// Encode the picked clips one after another (the GPU encoder is the bottleneck,
/// so in parallel would only split it). Returns the summary for the status bar.
pub fn export_clips(clips: Vec<(export::Job, String)>, dest: PathBuf, s: Settings, done: Arc<AtomicUsize>, prog: Arc<ClipProgress>, ctx: egui::Context) -> String {
    let n = clips.len();
    let (mut ok, mut skipped, mut errors) = (0, 0, Vec::new());
    let mut cancelled = false;
    for (k, (job, rel)) in clips.iter().enumerate() {
        if prog.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        *prog.label.lock().unwrap() = format!("{} clip {}/{n} · {rel}", if s.reencode { "Encoding" } else { "Copying" }, k + 1);
        prog.clip.store(0, Ordering::Relaxed);
        ctx.request_repaint();
        let out = export::target(&dest, rel, &s);
        match export::run(job, &s, &out, &prog.clip, &prog.cancel) {
            Ok(export::Outcome::Done) => ok += 1,
            Ok(export::Outcome::Skipped) => skipped += 1,
            Ok(export::Outcome::Cancelled) => {
                cancelled = true;
                break;
            }
            Err(e) => errors.push(format!("{rel}: {e}")),
        }
        done.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint();
    }
    let verb = if s.reencode { "encoded" } else { "copied" };
    let mut msg = format!("{ok} clip(s) {verb}");
    if skipped > 0 {
        msg += &format!(", {skipped} already there");
    }
    if cancelled {
        msg += ", cancelled";
    }
    if !errors.is_empty() {
        msg += &format!(", {} failed: {}", errors.len(), errors[0]);
    }
    msg
}

/// A ▶ and the length over the corner of a clip's thumbnail.
pub(crate) fn badge(ui: &egui::Ui, pic: Rect, meta: Option<&VideoMeta>, edit: &Edit, small: bool) {
    let text = match (meta, small) {
        (_, true) => "▶".to_string(),
        (Some(m), false) => {
            let (a, b) = edit.range(m.duration);
            format!("▶ {}{}", super::short_duration(b - a), if edit.trimmed() { " ✂" } else { "" })
        }
        (None, false) => "▶".into(),
    };
    let font = egui::FontId::proportional(if small { 9.0 } else { 12.0 });
    let galley = ui.painter().layout_no_wrap(text, font, Color32::WHITE);
    let pad = vec2(5.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let min = pos2(pic.right() - size.x - 4.0, pic.bottom() - size.y - 4.0);
    let r = Rect::from_min_size(min, size);
    ui.painter().rect_filled(r, 4.0, Color32::from_black_alpha(170));
    ui.painter().galley(r.min + pad, galley, Color32::WHITE);
}
