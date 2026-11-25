//! Horizon Studio `.mcpr` Replay & Cinematic Camera GUI Panel!
//! Directly drives Horizon's native `/studio` `.mcpr` packet recorder & replay player.
//! Press `K` (or `/replay_gui`) to open the in-game panel; `R` quick-toggles `.mcpr` recording; `B` adds a camera keyframe.

use mod_api::{
    self, ClientHost, ClientTickPostEvent, Corner, DrawContext, Flow, Hooks,
    InputKeyEvent, KeyAction, KeyConflictContext, KeyRegistration,
    Registrar, RegistrationContext, RenderGuiPostEvent,
    ScreenMouseButtonPressedPreEvent, ScreenMouseScrolledPreEvent, ScreenRenderPostEvent, Text,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const ID: &str = "replay_recorder";
const KEY_OPEN_GUI: &str = "open_gui";
const KEY_TOGGLE_REC: &str = "toggle_rec";
const KEY_ADD_KEYFRAME: &str = "add_keyframe";

const MARGIN: f32 = 8.0;
const ROW_H: f32 = 17.0;

fn panel_layout(sw: f32, sh: f32) -> (f32, f32, f32, f32, f32, f32, f32) {
    let gap = 8.0;
    let avail_w = (sw - MARGIN * 2.0 - gap).max(260.0);
    let left_w = (avail_w * 0.48).clamp(155.0, 200.0);
    let right_w = (avail_w - left_w).clamp(135.0, 220.0);
    let lx = MARGIN;
    let rx = (sw - right_w - MARGIN).max(lx + left_w + gap);
    let ly = MARGIN;
    let lh = (sh - MARGIN * 2.0).max(190.0);
    (lx, ly, left_w, rx, right_w, lh, (lh - 38.0).max(150.0))
}

static RECORDING_ACTIVE: AtomicBool = AtomicBool::new(false);
static FREECAM_ACTIVE: AtomicBool = AtomicBool::new(false);
static PATH_LOCKED: AtomicBool = AtomicBool::new(false);
static HUD_SHOWN: AtomicBool = AtomicBool::new(false);
static TICKS_RECORDED: AtomicU32 = AtomicU32::new(0);
static FILE_SCROLL: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_SPEED_IDX: AtomicU32 = AtomicU32::new(2); // 0=0.25x, 1=0.5x, 2=1.0x, 3=2.0x, 4=4.0x

const SPEEDS: [(&str, &str); 5] = [
    ("0.25x", "0.25"),
    ("0.5x", "0.5"),
    ("1x", "1.0"),
    ("2x", "2.0"),
    ("4x", "4.0"),
];

#[derive(Clone)]
struct McprEntry {
    name: String,
    size_label: String,
    modified_secs: u64,
}

static MCPR_LIST: OnceLock<Mutex<Vec<McprEntry>>> = OnceLock::new();
static PENDING_CMD: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static PENDING_CLOSE: AtomicBool = AtomicBool::new(false);

fn queue_studio(subcmd: String, close_gui: bool) {
    if let Ok(mut g) = PENDING_CMD.get_or_init(|| Mutex::new(None)).lock() {
        *g = Some(subcmd);
    }
    if close_gui {
        PENDING_CLOSE.store(true, Ordering::Relaxed);
    }
}

fn refresh_mcpr_list() {
    let mut entries = Vec::new();
    let dir = PathBuf::from("recordings");
    if let Ok(rd) = fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mcpr") {
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let meta = ent.metadata().ok();
                let bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let size_label = if bytes >= 1024 * 1024 {
                    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                } else {
                    format!("{} KB", (bytes / 1024).max(1))
                };
                let modified_secs = meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                entries.push(McprEntry {
                    name: name.to_string(),
                    size_label,
                    modified_secs,
                });
            }
        }
    }
    entries.sort_by(|a, b| b.modified_secs.cmp(&a.modified_secs));
    let lock = MCPR_LIST.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut g) = lock.lock() {
        *g = entries;
    }
}

fn snapshot_mcpr_list() -> Vec<McprEntry> {
    let lock = MCPR_LIST.get_or_init(|| Mutex::new(Vec::new()));
    lock.lock().map(|g| g.clone()).unwrap_or_default()
}

fn run_studio(host: &mut dyn ClientHost, subcmd: &str) {
    let cmd = format!("/studio {}", subcmd);
    if let Err(err) = host.send_chat(&cmd) {
        let _ = host.chat_line(Text::literal(format!("§c[Studio UI]§r 执行失败: {}", err)));
    }
}

fn toggle_recording(host: &mut dyn ClientHost) {
    let was = RECORDING_ACTIVE.load(Ordering::Relaxed);
    if !was {
        RECORDING_ACTIVE.store(true, Ordering::Relaxed);
        TICKS_RECORDED.store(0, Ordering::Relaxed);
        run_studio(host, "record start");
    } else {
        RECORDING_ACTIVE.store(false, Ordering::Relaxed);
        run_studio(host, "record stop");
        refresh_mcpr_list();
    }
}

fn open_replay_gui(host: &mut dyn ClientHost) -> bool {
    if host.mod_screen() == Some(ID) {
        host.mod_screen_close(ID);
        return false;
    }
    refresh_mcpr_list();
    FILE_SCROLL.store(0, Ordering::Relaxed);
    host.mod_screen_open(ID)
}

fn hit(mx: f32, my: f32, r: [f32; 4]) -> bool {
    mx >= r[0] && mx <= r[0] + r[2] && my >= r[1] && my <= r[1] + r[3]
}

fn visible_rows(sh: f32) -> usize {
    let avail = (sh - 56.0).max(ROW_H * 4.0);
    (avail / ROW_H).floor() as usize
}

fn draw_btn(draw: &mut dyn DrawContext, rect: [f32; 4], label: &str, active: bool, hovered: bool, accent: u32) {
    let bg = if active {
        accent
    } else if hovered {
        0xDD27_3549
    } else {
        0xBB16_1D2A
    };
    let fg = if active { 0xFFFF_FFFF } else { 0xFFE2_E8F0 };
    draw.fill(rect, bg);
    draw.fill([rect[0], rect[1], 2.5, rect[3]], if active { 0xFFFF_D700 } else { accent });
    let tw = draw.text_width(label);
    let tx = rect[0] + ((rect[2] - tw) * 0.5).max(6.0);
    let ty = rect[1] + (rect[3] - 8.0) * 0.5;
    draw.text(label, [tx, ty], fg, active);
}

#[derive(Default)]
struct ReplayRecorderMod;

#[mod_api::horizon_mod]
impl Hooks for ReplayRecorderMod {
    fn register(&self, r: &mut dyn Registrar, _: &RegistrationContext) {
        r.register_key_mapping(
            KeyRegistration::new(KEY_OPEN_GUI, "key.keyboard.k")
                .label("key.replay_recorder.open_gui")
                .context(KeyConflictContext::InGame),
        );
        r.register_key_mapping(
            KeyRegistration::new(KEY_TOGGLE_REC, "key.keyboard.r")
                .label("key.replay_recorder.toggle_rec")
                .context(KeyConflictContext::InGame),
        );
        r.register_key_mapping(
            KeyRegistration::new(KEY_ADD_KEYFRAME, "key.keyboard.b")
                .label("key.replay_recorder.add_keyframe")
                .context(KeyConflictContext::InGame),
        );
    }

    fn input_key(&self, host: &mut dyn ClientHost, event: &mut InputKeyEvent<'_>) {
        if !host.on(ID) || event.action != KeyAction::Press {
            return;
        }
        if event.key == "key.keyboard.k" && host.mod_screen() == Some(ID) {
            host.mod_screen_close(ID);
            return;
        }
        if !host.playing() {
            return;
        }
        if event.key == "key.keyboard.f9" {
            let prev = HUD_SHOWN.load(Ordering::Relaxed);
            HUD_SHOWN.store(!prev, Ordering::Relaxed);
        } else if event.key == "key.keyboard.k" && !host.is_down(ID, KEY_OPEN_GUI) {
            let _ = open_replay_gui(host);
        } else if event.key == "key.keyboard.r" && !host.is_down(ID, KEY_TOGGLE_REC) {
            toggle_recording(host);
        } else if event.key == "key.keyboard.b" && !host.is_down(ID, KEY_ADD_KEYFRAME) {
            run_studio(host, "keyframe add");
        }
    }

    fn client_tick_post(&self, host: &mut dyn ClientHost, _: &mut ClientTickPostEvent) {
        if RECORDING_ACTIVE.load(Ordering::Relaxed) {
            TICKS_RECORDED.fetch_add(1, Ordering::Relaxed);
        }
        if PENDING_CLOSE.swap(false, Ordering::Relaxed) && host.mod_screen() == Some(ID) {
            host.mod_screen_close(ID);
        }
        if let Some(subcmd) = PENDING_CMD
            .get_or_init(|| Mutex::new(None))
            .lock()
            .ok()
            .and_then(|mut g| g.take())
        {
            run_studio(host, &subcmd);
            if subcmd == "record stop" {
                refresh_mcpr_list();
            }
        }
        while host.consume_click(ID, KEY_OPEN_GUI) {
            let _ = open_replay_gui(host);
        }
        while host.consume_click(ID, KEY_TOGGLE_REC) {
            toggle_recording(host);
        }
        while host.consume_click(ID, KEY_ADD_KEYFRAME) {
            run_studio(host, "keyframe add");
        }
    }

    fn screen_render_post(&self, host: &dyn ClientHost, event: &mut ScreenRenderPostEvent<'_>) {
        if host.mod_screen() != Some(ID) {
            return;
        }
        let sw = event.screen.rect[2];
        let sh = event.screen.rect[3];
        let [mx, my] = event.screen.mouse;
        let draw: &mut dyn DrawContext = &mut *event.draw;

        let rec = RECORDING_ACTIVE.load(Ordering::Relaxed);
        let freecam = FREECAM_ACTIVE.load(Ordering::Relaxed);
        let path_locked = PATH_LOCKED.load(Ordering::Relaxed);
        let spd_idx = ACTIVE_SPEED_IDX.load(Ordering::Relaxed) as usize % SPEEDS.len();
        let secs = TICKS_RECORDED.load(Ordering::Relaxed) / 20;

        // === LEFT PANEL: Studio .mcpr Recording & Playback Controls ===
        let (lx, ly, left_w, rx, right_w, lh, _) = panel_layout(sw, sh);
        draw.fill([lx, ly, left_w, lh], 0xD20E_121B);
        draw.fill([lx, ly, left_w, 2.0], 0xFFFF_3B5C);
        draw.text("Studio 回放控制台", [lx + 7.0, ly + 6.0], 0xFFFF_D700, true);
        let status_txt = if rec {
            format!("● 录制中 {:02}:{:02}", secs / 60, secs % 60)
        } else {
            "官方 .mcpr 回放".to_string()
        };
        draw.text(
            &status_txt,
            [lx + 7.0, ly + 17.0],
            if rec { 0xFFFF_5577 } else { 0xFF94_A3B8 },
            false,
        );

        let bw = left_w - 14.0;
        let half = (bw - 4.0) * 0.5;
        let third = (bw - 6.0) / 3.0;
        let mut cy = ly + 30.0;

        // Record Start / Stop
        let r_rec_start = [lx + 7.0, cy, half, 17.0];
        let r_rec_stop = [lx + 7.0 + half + 4.0, cy, half, 17.0];
        draw_btn(draw, r_rec_start, "● 录制", rec, hit(mx, my, r_rec_start), 0xFFDC_2626);
        draw_btn(draw, r_rec_stop, "■ 停止", false, hit(mx, my, r_rec_stop), 0xFF64_748B);
        cy += 21.0;

        // Playback section
        draw.text("回放与倍速", [lx + 7.0, cy], 0xFF38_BDF8, true);
        cy += 11.0;
        let r_resume = [lx + 7.0, cy, third, 16.0];
        let r_pause = [lx + 7.0 + third + 3.0, cy, third, 16.0];
        let r_close = [lx + 7.0 + (third + 3.0) * 2.0, cy, third, 16.0];
        draw_btn(draw, r_resume, "▶ 播放", false, hit(mx, my, r_resume), 0xFF10_B981);
        draw_btn(draw, r_pause, "⏸ 暂停", false, hit(mx, my, r_pause), 0xFFF5_9E0B);
        draw_btn(draw, r_close, "✕ 退出", false, hit(mx, my, r_close), 0xFFEF_4444);
        cy += 19.0;

        // Speed buttons
        let sw_btn = (bw - 8.0) / 5.0;
        for (i, (lbl, _)) in SPEEDS.iter().enumerate() {
            let r = [lx + 7.0 + (sw_btn + 2.0) * i as f32, cy, sw_btn, 15.0];
            draw_btn(draw, r, lbl, i == spd_idx, hit(mx, my, r), 0xFF02_84C7);
        }
        cy += 20.0;

        // Cinematic Camera & Keyframes
        draw.text("运镜与关键帧", [lx + 7.0, cy], 0xFF38_BDF8, true);
        cy += 11.0;
        let r_freecam = [lx + 7.0, cy, bw, 16.0];
        draw_btn(
            draw,
            r_freecam,
            if freecam { "自由摄像机: 开" } else { "自由摄像机: 关" },
            freecam,
            hit(mx, my, r_freecam),
            0xFF7C_3AED,
        );
        cy += 19.0;

        let r_kf_add = [lx + 7.0, cy, half, 16.0];
        let r_kf_clr = [lx + 7.0 + half + 4.0, cy, half, 16.0];
        draw_btn(draw, r_kf_add, "+ 关键帧", false, hit(mx, my, r_kf_add), 0xFF0E_A5E9);
        draw_btn(draw, r_kf_clr, "清空点", false, hit(mx, my, r_kf_clr), 0xFF64_748B);
        cy += 19.0;

        let r_path = [lx + 7.0, cy, bw, 16.0];
        draw_btn(
            draw,
            r_path,
            if path_locked { "■ 停止运镜路径" } else { "▶ 沿关键帧播放" },
            path_locked,
            hit(mx, my, r_path),
            0xFF05_9669,
        );
        cy += 19.0;

        let r_editor = [lx + 7.0, cy, half, 16.0];
        let r_done = [lx + 7.0 + half + 4.0, cy, half, 16.0];
        draw_btn(draw, r_editor, "官方编辑器", false, hit(mx, my, r_editor), 0xFF4F_46E5);
        draw_btn(draw, r_done, "关闭 [K]", false, hit(mx, my, r_done), 0xFF47_5569);

        // === RIGHT PANEL: Local recordings/*.mcpr Browser ===
        let ry = ly;
        let rh = lh;
        draw.fill([rx, ry, right_w, rh], 0xD20E_121B);
        draw.fill([rx, ry, right_w, 2.0], 0xFF38_BDF8);
        let files = snapshot_mcpr_list();
        draw.text(&format!("录像 ({})", files.len()), [rx + 7.0, ry + 6.0], 0xFFFF_D700, true);
        let r_refresh = [rx + right_w - 40.0, ry + 4.0, 34.0, 14.0];
        draw_btn(draw, r_refresh, "↻刷新", false, hit(mx, my, r_refresh), 0xFF02_84C7);
        draw.text("点击直接回放 · 滚轮翻页", [rx + 7.0, ry + 18.0], 0xFF94_A3B8, false);

        let vis = visible_rows(sh);
        let max_scroll = files.len().saturating_sub(vis);
        let scroll = FILE_SCROLL.load(Ordering::Relaxed).min(max_scroll);
        if files.is_empty() {
            draw.text("暂无录像，点击左侧「录制」", [rx + 8.0, ry + 40.0], 0xFF64_748B, false);
        } else {
            let mut fy = ry + 32.0;
            let max_chars = ((right_w - 52.0) / 5.5).max(10.0) as usize;
            for ent in files.iter().skip(scroll).take(vis) {
                let r = [rx + 5.0, fy, right_w - 10.0, ROW_H - 2.0];
                let hov = hit(mx, my, r);
                draw.fill(r, if hov { 0xDD1E_3A5F } else { 0xAA16_1D2A });
                draw.fill([r[0], r[1], 2.0, r[3]], if hov { 0xFF38_BDF8 } else { 0xFF33_4155 });
                let short_name: String = ent.name.chars().take(max_chars).collect();
                draw.text(&short_name, [r[0] + 5.0, r[1] + 4.0], 0xFFF1_F5F9, hov);
                let sw_lbl = draw.text_width(&ent.size_label);
                draw.text(&ent.size_label, [r[0] + r[2] - sw_lbl - 4.0, r[1] + 4.0], 0xFF94_A3B8, false);
                fy += ROW_H;
            }
        }
    }

    fn screen_mouse_button_pressed_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseButtonPressedPreEvent<'_>,
    ) -> Flow {
        if host.mod_screen() != Some(ID) || event.button != 0 {
            return Flow::Continue;
        }
        let sw = event.screen.rect[2];
        let sh = event.screen.rect[3];
        let [mx, my] = event.screen.mouse;

        let (lx, ly, left_w, rx, right_w, _, _) = panel_layout(sw, sh);
        let bw = left_w - 14.0;
        let half = (bw - 4.0) * 0.5;
        let third = (bw - 6.0) / 3.0;
        let mut cy = ly + 30.0;

        if hit(mx, my, [lx + 7.0, cy, half, 17.0]) {
            RECORDING_ACTIVE.store(true, Ordering::Relaxed);
            TICKS_RECORDED.store(0, Ordering::Relaxed);
            queue_studio("record start".into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        if hit(mx, my, [lx + 7.0 + half + 4.0, cy, half, 17.0]) {
            RECORDING_ACTIVE.store(false, Ordering::Relaxed);
            queue_studio("record stop".into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        cy += 32.0;

        if hit(mx, my, [lx + 7.0, cy, third, 16.0]) {
            queue_studio("replay resume".into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        if hit(mx, my, [lx + 7.0 + third + 3.0, cy, third, 16.0]) {
            queue_studio("replay pause".into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        if hit(mx, my, [lx + 7.0 + (third + 3.0) * 2.0, cy, third, 16.0]) {
            queue_studio("replay close".into(), true);
            event.cancelled = true;
            return Flow::Continue;
        }
        cy += 19.0;

        let sw_btn = (bw - 8.0) / 5.0;
        for (i, (_, val)) in SPEEDS.iter().enumerate() {
            let r = [lx + 7.0 + (sw_btn + 2.0) * i as f32, cy, sw_btn, 15.0];
            if hit(mx, my, r) {
                ACTIVE_SPEED_IDX.store(i as u32, Ordering::Relaxed);
                queue_studio(format!("replay speed {}", val), false);
                event.cancelled = true;
                return Flow::Continue;
            }
        }
        cy += 31.0;

        if hit(mx, my, [lx + 7.0, cy, bw, 16.0]) {
            let next = !FREECAM_ACTIVE.load(Ordering::Relaxed);
            FREECAM_ACTIVE.store(next, Ordering::Relaxed);
            queue_studio(if next { "freecam on" } else { "freecam off" }.into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        cy += 19.0;

        if hit(mx, my, [lx + 7.0, cy, half, 16.0]) {
            queue_studio("keyframe add".into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        if hit(mx, my, [lx + 7.0 + half + 4.0, cy, half, 16.0]) {
            queue_studio("keyframe clear".into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        cy += 19.0;

        if hit(mx, my, [lx + 7.0, cy, bw, 16.0]) {
            let next = !PATH_LOCKED.load(Ordering::Relaxed);
            PATH_LOCKED.store(next, Ordering::Relaxed);
            queue_studio(if next { "path lock" } else { "path unlock" }.into(), false);
            event.cancelled = true;
            return Flow::Continue;
        }
        cy += 19.0;

        if hit(mx, my, [lx + 7.0, cy, half, 16.0]) {
            queue_studio("screen open".into(), true);
            event.cancelled = true;
            return Flow::Continue;
        }
        if hit(mx, my, [lx + 7.0 + half + 4.0, cy, half, 16.0]) {
            PENDING_CLOSE.store(true, Ordering::Relaxed);
            event.cancelled = true;
            return Flow::Continue;
        }

        // Right panel clicks
        let ry = ly;
        if hit(mx, my, [rx + right_w - 40.0, ry + 4.0, 34.0, 14.0]) {
            refresh_mcpr_list();
            event.cancelled = true;
            return Flow::Continue;
        }
        let files = snapshot_mcpr_list();
        let vis = visible_rows(sh);
        let max_scroll = files.len().saturating_sub(vis);
        let scroll = FILE_SCROLL.load(Ordering::Relaxed).min(max_scroll);
        let mut fy = ry + 32.0;
        for ent in files.iter().skip(scroll).take(vis) {
            let r = [rx + 5.0, fy, right_w - 10.0, ROW_H - 2.0];
            if hit(mx, my, r) {
                let path = format!("recordings/{}", ent.name);
                queue_studio(format!("replay open {}", path), true);
                event.cancelled = true;
                return Flow::Continue;
            }
            fy += ROW_H;
        }

        Flow::Continue
    }

    fn screen_mouse_scrolled_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseScrolledPreEvent<'_>,
    ) -> Flow {
        if host.mod_screen() != Some(ID) {
            return Flow::Continue;
        }
        let files = snapshot_mcpr_list();
        let vis = visible_rows(event.screen.rect[3]);
        let max_scroll = files.len().saturating_sub(vis);
        let cur = FILE_SCROLL.load(Ordering::Relaxed);
        let next = if event.scroll[1] < 0.0 {
            (cur + 3).min(max_scroll)
        } else if event.scroll[1] > 0.0 {
            cur.saturating_sub(3)
        } else {
            cur
        };
        FILE_SCROLL.store(next, Ordering::Relaxed);
        event.cancelled = true;
        Flow::Continue
    }

    fn render_gui_post(&self, host: &dyn ClientHost, event: &mut RenderGuiPostEvent<'_>) {
        if !host.on(ID) || (!HUD_SHOWN.load(Ordering::Relaxed) && !RECORDING_ACTIVE.load(Ordering::Relaxed)) {
            return;
        }
        let active = RECORDING_ACTIVE.load(Ordering::Relaxed);
        let secs = TICKS_RECORDED.load(Ordering::Relaxed) / 20;
        let line1 = if active {
            format!("● REC [.mcpr] {:02}:{:02} [K面板 / R停止]", secs / 60, secs % 60)
        } else {
            "Studio 回放就绪 [按K打开面板 / R录制]".to_string()
        };
        let draw = &mut *event.draw;
        let w = draw.text_width(&line1) + 8.0;
        let h = 14.0;
        let (x, y) = event.corners.place(Corner::BottomRight, w, h);
        draw.fill([x, y, w, h], 0x8818_1014);
        let accent = if active { 0xFFFF_2B4A } else { 0xFF38_BDF8 };
        draw.fill([x, y, 2.5, h], accent);
        draw.text(&line1, [x + 5.0, y + 3.0], accent, true);
    }
}
