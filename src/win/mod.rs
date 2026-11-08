//! Windows shell: a widget embedded in the taskbar, a tray icon as fallback, and the
//! polling loop that feeds them.
//!
//! The widget is a layered child window of `Shell_TrayWnd`, so it shares an input queue
//! with Explorer: nothing on the UI thread may block. Network requests run on a worker
//! thread and come back through `WM_APP_FETCHED`.

mod autostart;
mod http;
mod popup;
mod render;
mod web_login;

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, ReleaseCapture, SetCapture, LASTINPUTINFO, VK_ESCAPE,
};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::alert::{self, Alert};
use crate::cache::{self, Cache, Entry, Stored};
use crate::model::{FetchError, Provider, Thresholds, Usage};
use crate::panel::{self, Panel};
use crate::providers::{self, Env};
use crate::schedule;
use crate::settings::Settings;
use crate::view::{self, Severity, View};
use render::{draw_text, Align, Color, Font, Surface};

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_FETCHED: u32 = WM_APP + 2;
const WM_APP_WEB_LOGIN: u32 = WM_APP + 3;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const TICK_TIMER: usize = 1;
const TICK_MS: u32 = 2000;
const TRAY_ID: u32 = 1;

const CMD_CLAUDE: u32 = 1;
const CMD_CODEX: u32 = 2;
const CMD_USED: u32 = 3;
const CMD_REMAINING: u32 = 4;
const CMD_AUTOSTART: u32 = 6;
const CMD_NOTIFY: u32 = 7;
const CMD_EXIT: u32 = 9;
const CMD_WEB_CONFIG: u32 = 12;

/// With no keyboard or mouse input for this long, nobody is looking at the widget.
const IDLE_AFTER_SECS: i64 = 300;

// Widget layout in 96-dpi pixels, left to right: grip, provider badge, row label, bar, text.
const PAD_L: f32 = 2.0;
const GRIP_W: f32 = 8.0;
const BADGE_X: f32 = 14.0;
const BADGE_W: f32 = 12.0;
const LABEL_X: f32 = 30.0;
const LABEL_W: f32 = 18.0;
const BAR_X: f32 = 49.0;
const BAR_W: f32 = 96.0;
const TEXT_X: f32 = 151.0;
const TEXT_W: f32 = 54.0;
const WIDTH: f32 = 207.0;
const BAR_H: f32 = 9.0;
const BAR_RADIUS: f32 = 1.5;
const ROW_GAP: f32 = 6.0;
const FONT_PX: f32 = 10.0;
const BADGE_FONT_PX: f32 = 12.0;

const FILL_OK: Color = Color::rgb(34, 197, 94);
const FILL_WARN: Color = Color::rgb(251, 191, 36);
const FILL_CRIT: Color = Color::rgb(239, 68, 68);
/// How much stale numbers are dimmed.
const STALE_FADE: f32 = 0.55;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

struct Instance(HANDLE);

impl Instance {
    fn acquire() -> Option<Self> {
        let handle = unsafe { CreateMutexW(null(), 0, wide("Local\\IUsage.SingleInstance").as_ptr()) };
        if handle.is_null() {
            return None;
        }
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let instance = Self(handle);
        (!exists).then_some(instance)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies `text` into a fixed-size UTF-16 field, truncating and keeping the terminator.
fn copy_wide(field: &mut [u16], text: &str) {
    let limit = field.len() - 1;
    for (slot, unit) in field.iter_mut().zip(text.encode_utf16().take(limit)) {
        *slot = unit;
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
}

fn app_data_dir() -> PathBuf {
    std::env::var_os("APPDATA").map_or_else(home_dir, PathBuf::from)
}

/// The Claude desktop app is a packaged (MSIX) app, so what it writes to `%APPDATA%`
/// really lands in its package folder; the plain path only exists for programs started
/// from inside the desktop app. Both are looked at.
fn provider_env() -> Arc<Env> {
    const PACKAGED: &str = "Packages\\Claude_pzs8sxrjxfjjc\\LocalCache\\Roaming";
    let local = std::env::var_os("LOCALAPPDATA").map_or_else(|| home_dir().join("AppData\\Local"), PathBuf::from);
    let claude_code_dirs =
        [app_data_dir(), local.join(PACKAGED)].into_iter().map(|dir| dir.join("Claude").join("claude-code")).collect();
    let home = home_dir();
    Arc::new(Env { claude_dir: providers::claude_dir(&home), home, claude_code_dirs })
}

/// Seconds since the last keyboard or mouse input anywhere on the desktop.
fn idle_secs() -> i64 {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut info) == 0 {
            return 0;
        }
        // Both are 32-bit millisecond tick counts that wrap together.
        (GetTickCount().wrapping_sub(info.dwTime) / 1000) as i64
    }
}

/// Whether the provider's sign-in has changed since it was found without paid quota at
/// `unpaid`. A Claude web login is not a file of theirs: updating it in our own dialog
/// clears the pause directly.
fn login_changed(provider: Provider, env: &Env, settings: &Settings, unpaid: i64) -> bool {
    if provider == Provider::Claude && web_login::is_web(settings) {
        return false;
    }
    providers::signed_in_at(provider, env).is_some_and(|at| at > unpaid)
}

/// Runs `f` on the app state. Returns `None` when the state is already borrowed, which
/// happens when a Win32 call made from inside a handler re-enters a window procedure.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

/// What is currently on screen; repainting is skipped while this is unchanged.
#[derive(Clone, PartialEq)]
struct Shown {
    view: View,
    light: bool,
}

#[derive(Clone, Copy, PartialEq, Default)]
struct Geom {
    /// Widget position in taskbar client coordinates.
    x: i32,
    w: i32,
    h: i32,
    /// X at which the widget sits flush against the tray; the saved offset counts back from it.
    anchor: i32,
    scale: f32,
}

struct Drag {
    grip: bool,
    moved: bool,
    start_cursor: i32,
    start_x: i32,
}

/// A job for the cache writer. A request worker waits for the acknowledgement of its
/// save, so its cooldown is on disk before anything is sent.
enum Save {
    Write(Stored, Option<SyncSender<std::io::Result<()>>>),
    Stop,
}

/// Writes `cache.json` on a thread of its own, in the order the saves were asked for.
/// Each save is flushed to disk, which can take tens of milliseconds; the UI thread
/// shares its input queue with Explorer and must not wait for that.
fn start_saver(path: PathBuf) -> Option<(Sender<Save>, JoinHandle<()>)> {
    let (sender, jobs) = mpsc::channel::<Save>();
    let writer = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
        for job in jobs {
            let Save::Write(stored, ack) = job else { break };
            let result = cache::save(&path, &stored);
            if let Some(ack) = ack {
                let _ = ack.send(result);
            }
        }
    });
    writer.ok().map(|writer| (sender, writer))
}

/// The detail panel while it is open.
struct Popup {
    hwnd: HWND,
    surface: Surface,
    scratch: Surface,
    scale: f32,
    /// What the surface currently holds; repainting is skipped while this is unchanged.
    shown: Option<(Panel, bool)>,
}

struct App {
    main: HWND,
    widget: HWND,
    geom: Geom,
    surface: Option<Surface>,
    scratch: Option<Surface>,
    settings: Settings,
    settings_path: PathBuf,
    cache_path: PathBuf,
    /// The cache writer; `None` if its thread could not be started, and saves then run
    /// where they are asked for.
    saver: Option<(Sender<Save>, JoinHandle<()>)>,
    error_path: PathBuf,
    env: Arc<Env>,
    // Per-provider readings, errors and persisted request timing.
    cache: Cache,
    error: [Option<FetchError>; 2],
    schedule: [schedule::State; 2],
    fetching: Option<(Provider, JoinHandle<Result<(Usage, cache::Source), FetchError>>)>,
    discard_claude_result: bool,
    web_expired: bool,
    /// See `Stored::unpaid`.
    unpaid: [i64; 2],
    light: bool,
    shown: Option<Shown>,
    drag: Option<Drag>,
    icon: HICON,
    popup: Option<Popup>,
    popup_closed_at: Option<Instant>,
}

/// Fetches both providers once and prints the result. `iusage --check > out.txt`
pub fn check() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let Some(_instance) = Instance::acquire() else {
        let _ = writeln!(out, "IUsage is already running; use its cached panel or refresh menu.");
        return;
    };
    let env = provider_env();
    let dir = app_data_dir().join("IUsage");
    let path = dir.join("cache.json");
    let mut stored = cache::load(&path);
    let settings = Settings::load(&dir.join("settings.json"));
    for provider in Provider::ALL {
        if provider == Provider::Claude && stored.web_expired && web_login::is_web(&settings) {
            let _ = writeln!(out, "Claude: web login expired; update login information in the menu.");
            continue;
        }
        let now = unix_now();
        let i = provider.index();
        if stored.unpaid[i] != 0 && !login_changed(provider, &env, &settings, stored.unpaid[i]) {
            let _ = writeln!(out, "{}: no paid quota; refresh paused until its sign-in changes.", provider.name());
            continue;
        }
        if !stored.schedule[i].can_request(now) {
            let _ = writeln!(out, "{}: skipped (request cooldown)", provider.name());
            continue;
        }
        if stored.cache[i].is_some_and(|e| (0..i64::from(settings.poll_secs)).contains(&now.saturating_sub(e.fetched_at))) {
            let v = view::build(provider, stored.cache[i].as_ref().map(|e| &e.usage), None, false, settings.thresholds, now);
            let _ = writeln!(out, "{} (cached): 5h {} | 7d {}", provider.name(), v.rows[0].text, v.rows[1].text);
            continue;
        }
        stored.schedule[i].started(now);
        if cache::save(&path, &stored).is_err() {
            let _ = writeln!(out, "{}: skipped (cannot persist request cooldown)", provider.name());
            continue;
        }
        let session = if provider == Provider::Claude { web_login::selected(&settings) } else { Ok(None) };
        let result = session.and_then(|session| providers::fetch_selected(provider, &env, now, http::send, session.as_ref(), &mut || Ok(())));
        let line = match result {
            Ok((usage, source)) => {
                let now = unix_now();
                stored.schedule[i].succeeded(now, settings.poll_secs);
                let mut announced = stored.cache[i].map_or([0, 0], |e| e.announced);
                if let Some(previous) = stored.cache[i] {
                    alert::rearm(&mut announced, &previous.usage, &usage, now);
                }
                stored.cache[i] = Some(Entry { usage, source, fetched_at: now, announced });
                stored.unpaid[i] = 0;
                let v = view::build(provider, Some(&usage), None, false, Thresholds::default(), now);
                format!("{}: 5h {} | 7d {}", provider.name(), v.rows[0].text, v.rows[1].text)
            }
            Err(FetchError::Unsubscribed) => {
                let now = unix_now();
                stored.schedule[i].succeeded(now, settings.poll_secs);
                stored.unpaid[i] = now;
                stored.cache[i] = None;
                format!("{}: no paid quota; refresh paused", provider.name())
            }
            Err(error) => {
                stored.schedule[i].failed(&error, unix_now());
                if provider == Provider::Claude { cache::mark_web_expired(&mut stored.web_expired, &error); }
                format!("{}: {error:?}", provider.name())
            }
        };
        let _ = cache::save(&path, &stored);
        let _ = writeln!(out, "{line}");
    }
}

pub fn run() {
    let Some(_instance) = Instance::acquire() else { return };
    unsafe {
        let instance = GetModuleHandleW(null());
        let main_class = wide("IUsageMain");
        let widget_class = wide("IUsageWidget");
        let popup_class = wide("IUsagePopup");
        let classes: [(&Vec<u16>, WNDPROC); 3] =
            [(&main_class, Some(main_proc)), (&widget_class, Some(widget_proc)), (&popup_class, Some(popup_proc))];
        for (name, proc) in classes {
            let mut class: WNDCLASSW = std::mem::zeroed();
            class.lpfnWndProc = proc;
            class.hInstance = instance;
            class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
            class.lpszClassName = name.as_ptr();
            RegisterClassW(&class);
        }

        // A hidden top-level window: broadcasts such as TaskbarCreated only reach those.
        let main = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            main_class.as_ptr(),
            wide("IUsage").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if main.is_null() {
            return;
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);

        let settings_dir = app_data_dir().join("IUsage");
        let settings_path = settings_dir.join("settings.json");
        let cache_path = settings_dir.join("cache.json");
        let settings = Settings::load(&settings_path);
        let Stored { cache, mut schedule, web_expired, unpaid } = cache::load(&cache_path);
        // A reading that is still fresh is shown as is; nothing is requested until it
        // ages out. Times in the future are capped so a clock that was set back cannot
        // stall us. A wait the service imposed on an earlier run carries over as loaded.
        let now = unix_now();
        let poll = settings.poll_secs as i64;
        for i in 0..2 {
            if let Some(entry) = cache[i] {
                schedule[i].next_poll = schedule[i].next_poll.max(entry.fetched_at.saturating_add(poll).min(now + poll));
            }
        }
        let mut errors = schedule.map(|gate| (gate.backoff > 0).then_some(FetchError::PreviousFailure));
        for (error, since) in errors.iter_mut().zip(unpaid) {
            if since != 0 { *error = Some(FetchError::Unsubscribed); }
        }
        // An expired web login is the thing to act on first.
        if web_expired && web_login::is_web(&settings) { errors[0] = Some(FetchError::WebUnauthorized); }
        let app = App {
            main,
            widget: null_mut(),
            geom: Geom::default(),
            surface: None,
            scratch: None,
            settings,
            settings_path,
            saver: start_saver(cache_path.clone()),
            cache_path,
            error_path: settings_dir.join("last-error.txt"),
            env: provider_env(),
            cache,
            error: errors,
            schedule,
            fetching: None,
            discard_claude_result: false,
            web_expired,
            unpaid,
            light: system_uses_light_theme(),
            shown: None,
            drag: None,
            icon: null_mut(),
            popup: None,
            popup_closed_at: None,
        };
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        with_app(|app| {
            app.set_tray(NIM_ADD);
            app.tick();
        });
        SetTimer(main, TICK_TIMER, TICK_MS, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        with_app(|app| app.shutdown());
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam == TICK_TIMER => {
            with_app(|app| app.tick());
            0
        }
        WM_APP_FETCHED => {
            with_app(|app| app.on_fetched());
            0
        }
        WM_APP_WEB_LOGIN => {
            with_app(|app| app.set_claude_source(wparam == 1));
            0
        }
        WM_APP_TRAY => {
            match lparam as u32 & 0xFFFF {
                WM_LBUTTONUP => {
                    with_app(|app| app.toggle_popup());
                }
                WM_RBUTTONUP => menu(hwnd),
                NIN_BALLOONUSERCLICK => { with_app(|app| { if app.web_expired { app.open_web_config(); } }); }
                _ => {}
            }
            0
        }
        WM_POWERBROADCAST => {
            if wparam == PBT_APMRESUMEAUTOMATIC {
                // Timers do not count time spent asleep: fetch as soon as we are back.
                // The minimum gap and any wait the service imposed still apply.
                with_app(|app| app.schedule.iter_mut().for_each(|s| s.next_poll = 0));
            }
            1
        }
        WM_SETTINGCHANGE => {
            with_app(|app| {
                app.light = system_uses_light_theme();
                app.refresh();
            });
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer restarted: the tray icon has to be registered again, and the old
            // widget died with the old taskbar. `tick` notices a dead widget by itself;
            // forgetting a live one here would leave a second widget on the taskbar,
            // because this broadcast also arrives when the taskbar was not replaced.
            with_app(|app| {
                app.set_tray(NIM_ADD);
                app.tick();
            });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let client_x = (lparam & 0xFFFF) as i16 as i32;
    match msg {
        WM_LBUTTONDOWN => {
            with_app(|app| app.on_down(client_x));
            0
        }
        WM_MOUSEMOVE => {
            with_app(|app| app.on_move());
            0
        }
        WM_LBUTTONUP => {
            with_app(|app| app.on_up(client_x));
            0
        }
        WM_CAPTURECHANGED => {
            with_app(|app| app.on_capture_lost());
            0
        }
        WM_RBUTTONUP => {
            if let Some(main) = with_app(|app| app.main) {
                menu(main);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONUP => {
            with_app(|app| {
                let hit = app.popup.as_ref().and_then(|p| p.shown.as_ref().and_then(|(panel, _)|
                    popup::link_hit(panel, (lparam & 0xffff) as i16 as i32, ((lparam >> 16) & 0xffff) as i16 as i32, p.scale)
                        .map(|provider| (provider, panel[provider.index()].update_login))));
                match hit {
                    Some((_, true)) => { app.open_web_config(); app.close_popup(); }
                    Some((provider, false)) => app.recheck(provider),
                    None => {}
                }
            });
            0
        }
        WM_KEYDOWN if wparam == 13 => {
            with_app(|app| {
                if app.popup.as_ref().is_some_and(|p| p.shown.as_ref().is_some_and(|(panel, _)| panel[0].update_login)) {
                    app.open_web_config(); app.close_popup();
                }
            });
            0
        }
        WM_PAINT => {
            if with_app(|app| app.blit_popup(hwnd)).is_none() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            0
        }
        WM_ERASEBKGND => 1,
        // Clicking anywhere else, or Escape, dismisses the panel. Both are ignored while
        // the app state is borrowed, which is when we are the ones closing it.
        WM_ACTIVATE if wparam & 0xFFFF == WA_INACTIVE as usize => {
            with_app(|app| app.close_popup());
            0
        }
        WM_KEYDOWN if wparam == VK_ESCAPE as usize => {
            with_app(|app| app.close_popup());
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Seconds east of UTC for the local time zone, daylight saving included.
unsafe fn utc_offset_secs() -> i64 {
    let mut zone: TIME_ZONE_INFORMATION = std::mem::zeroed();
    let bias = match GetTimeZoneInformation(&mut zone) {
        1 => zone.Bias + zone.StandardBias,
        2 => zone.Bias + zone.DaylightBias,
        _ => zone.Bias,
    };
    -(bias as i64) * 60
}

/// Shows the context menu at the cursor and applies the choice. The menu runs its own
/// message loop, so the app state must not be borrowed while it is open.
unsafe fn menu(owner: HWND) {
    let Some((provider, show_remaining, notify, web, expired)) = with_app(|app| {
        (app.settings.provider, app.settings.show_remaining, app.settings.notify, web_login::is_web(&app.settings), app.web_expired)
    }) else {
        return;
    };
    let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let menu = CreatePopupMenu();
    let item = |id: u32, label: &str| {
        AppendMenuW(menu, MF_STRING, id as usize, wide(label).as_ptr());
    };
    item(CMD_CLAUDE, "Claude");
    item(CMD_CODEX, "Codex");
    item(CMD_WEB_CONFIG, if web && expired { "Claude 登录已失效：更新…" } else { "Claude 登录设置…" });
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    item(CMD_USED, "显示已用");
    item(CMD_REMAINING, "显示剩余");
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING | checked(notify), CMD_NOTIFY as usize, wide("低额度提醒").as_ptr());
    AppendMenuW(menu, MF_STRING | checked(autostart::is_enabled()), CMD_AUTOSTART as usize, wide("开机自启").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    // Informational only: greyed, so it cannot be chosen.
    let version = concat!("IUsage 版本 ", env!("CARGO_PKG_VERSION"));
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(version).as_ptr());
    item(CMD_EXIT, "退出");
    let current = if provider == Provider::Claude { CMD_CLAUDE } else { CMD_CODEX };
    CheckMenuRadioItem(menu, CMD_CLAUDE, CMD_CODEX, current, MF_BYCOMMAND);
    let mode = if show_remaining { CMD_REMAINING } else { CMD_USED };
    CheckMenuRadioItem(menu, CMD_USED, CMD_REMAINING, mode, MF_BYCOMMAND);

    let mut cursor = POINT { x: 0, y: 0 };
    GetCursorPos(&mut cursor);
    // Without a foreground owner the menu would not close when clicking elsewhere.
    SetForegroundWindow(owner);
    let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN;
    let command = TrackPopupMenu(menu, flags, cursor.x, cursor.y, 0, owner, null()) as u32;
    PostMessageW(owner, WM_NULL, 0, 0);
    DestroyMenu(menu);

    if command == CMD_EXIT {
        DestroyWindow(owner);
    } else if command != 0 {
        with_app(|app| app.on_command(command));
    }
}

unsafe fn system_uses_light_theme() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("SystemUsesLightTheme");
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = RegGetValueW(
        HKEY_CURRENT_USER,
        key.as_ptr(),
        name.as_ptr(),
        RRF_RT_REG_DWORD,
        null_mut(),
        &mut value as *mut u32 as *mut c_void,
        &mut size,
    );
    status == ERROR_SUCCESS && value != 0
}

fn fill_color(severity: Severity) -> Color {
    match severity {
        Severity::Ok => FILL_OK,
        Severity::Warn => FILL_WARN,
        Severity::Crit => FILL_CRIT,
    }
}

fn badge(provider: Provider) -> (&'static str, Color) {
    match provider {
        Provider::Claude => ("C", Color::rgb(217, 119, 87)),
        Provider::Codex => ("X", Color::rgb(122, 162, 247)),
    }
}

/// Text and track colours for the taskbar's theme.
fn theme_colors(light: bool) -> (Color, Color) {
    if light {
        (Color::rgba(0, 0, 0, 190), Color::rgba(0, 0, 0, 50))
    } else {
        (Color::rgba(255, 255, 255, 200), Color::rgba(255, 255, 255, 56))
    }
}

/// The application icon embedded by `build.rs` (icon group 1), at the tray's size.
/// Usage is shown by the widget and tooltip, so the tray icon never changes.
unsafe fn load_app_icon() -> HICON {
    let n = GetSystemMetrics(SM_CXSMICON).max(16);
    // MAKEINTRESOURCE(1): the resource id travels in the pointer value.
    LoadImageW(GetModuleHandleW(null()), 1 as *const u16, IMAGE_ICON, n, n, LR_DEFAULTCOLOR) as HICON
}

impl App {
    fn save_settings(&self) {
        // Losing a preference is not worth interrupting the user for.
        let _ = self.settings.save(&self.settings_path);
    }

    unsafe fn tick(&mut self) {
        // Also consume completion here if PostMessage failed or arrived just before
        // the worker returned. Never abandon a live worker and launch overlapping I/O.
        self.on_fetched();
        self.ensure_widget();
        self.fetch_if_due();
        self.refresh();
    }

    /// Asks for the displayed provider once its reading has aged out, and for the other
    /// one as well while the detail panel is open. One request runs at a time; the next
    /// is picked up when the first one returns.
    fn fetch_if_due(&mut self) {
        let now = unix_now();
        let current = self.settings.provider;
        let wanted = [Some(current), self.popup.is_some().then_some(current.other())];
        // An open panel means someone is looking, whatever the input timer says.
        let away = self.popup.is_none() && idle_secs() >= IDLE_AFTER_SECS;
        for provider in wanted.into_iter().flatten() {
            let i = provider.index();
            let last_success = self.cache[i].map(|entry| entry.fetched_at);
            if !self.schedule[i].due(last_success, away, now) {
                continue;
            }
            // An account without paid quota is asked again only once its sign-in changes,
            // or when the user asks. Until then its file is looked at once per interval.
            if self.unpaid[i] != 0 && !login_changed(provider, &self.env, &self.settings, self.unpaid[i]) {
                self.schedule[i].next_poll = now + i64::from(self.settings.poll_secs);
                continue;
            }
            self.start_fetch(provider);
        }
    }

    /// The user says the account may be paid again: ask once more, as soon as the request
    /// gates allow. No paid quota found then pauses it again.
    unsafe fn recheck(&mut self, provider: Provider) {
        let i = provider.index();
        if self.unpaid[i] == 0 {
            return;
        }
        self.unpaid[i] = 0;
        self.schedule[i].next_poll = 0;
        self.fetch_if_due();
        self.refresh();
    }

    fn stored(&self) -> Stored {
        Stored { cache: self.cache, schedule: self.schedule, web_expired: self.web_expired, unpaid: self.unpaid }
    }

    fn save_cache(&self) {
        // Best effort: without the file a restart just fetches again.
        if let Some((saver, _)) = &self.saver {
            if saver.send(Save::Write(self.stored(), None)).is_ok() {
                return;
            }
        }
        let _ = cache::save(&self.cache_path, &self.stored());
    }

    unsafe fn toggle_popup(&mut self) {
        if self.popup.is_some() {
            self.close_popup();
            return;
        }
        // A click on the widget or the tray icon first takes focus from an open panel,
        // which closes it; that same click must not open it again.
        if self.popup_closed_at.is_some_and(|at| at.elapsed() < Duration::from_millis(300)) {
            return;
        }
        self.open_popup();
    }

    unsafe fn open_popup(&mut self) {
        let scale = if self.geom.scale > 0.0 {
            self.geom.scale
        } else {
            GetDpiForWindow(self.main).max(96) as f32 / 96.0
        };
        let (w, h) = popup::size(scale, &self.panel());
        let (Some(surface), Some(scratch)) = (Surface::new(w, h), Surface::new(w, h)) else {
            return;
        };

        // Centred on the click, just above the taskbar, kept inside the monitor's work area.
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        let mut monitor: MONITORINFO = std::mem::zeroed();
        monitor.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST), &mut monitor);
        let work = monitor.rcWork;
        let margin = (8.0 * scale).round() as i32;
        let x = (cursor.x - w / 2).min(work.right - w - margin).max(work.left + margin);
        let y = work.bottom - h - margin;

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            wide("IUsagePopup").as_ptr(),
            wide("IUsage").as_ptr(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        // Windows 11 rounded corners and shadow; a dark frame in dark mode.
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const u32 as *const c_void, 4); // DWMWA_WINDOW_CORNER_PREFERENCE
        let dark: i32 = (!self.light).into();
        DwmSetWindowAttribute(hwnd, 20, &dark as *const i32 as *const c_void, 4); // DWMWA_USE_IMMERSIVE_DARK_MODE

        self.popup = Some(Popup { hwnd, surface, scratch, scale, shown: None });
        self.refresh_popup();
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        self.fetch_if_due();
    }

    unsafe fn close_popup(&mut self) {
        if let Some(popup) = self.popup.take() {
            DestroyWindow(popup.hwnd);
            self.popup_closed_at = Some(Instant::now());
        }
    }

    /// What the detail panel shows right now.
    unsafe fn panel(&self) -> Panel {
        let context = panel::Context {
            current: self.settings.provider,
            show_remaining: self.settings.show_remaining,
            thresholds: self.settings.thresholds,
            now: unix_now(),
            utc_offset: utc_offset_secs(),
        };
        let mut sections = panel::build(&self.cache, &self.error, &context);
        for section in &mut sections {
            let gate = self.schedule[section.provider.index()];
            // A request in flight holds only its own gap, not a wait worth a row: showing it
            // would grow the panel by a line until the reply arrives, then shrink it again.
            let in_flight = self.fetching.as_ref().is_some_and(|(p, _)| *p == section.provider);
            if !in_flight && schedule::told_to_wait(gate.blocked_until, context.now) {
                section.retry = format!("等待至 {} · {}", crate::timefmt::clock(gate.blocked_until, context.now, context.utc_offset),
                    crate::timefmt::countdown_long(gate.blocked_until - context.now));
                if gate.backoff > 0 && self.error[section.provider.index()].is_none() {
                    section.status = "上次请求失败，保留缓存并继续等待".into();
                    section.failed = true;
                }
            }
            if section.provider == Provider::Claude && self.web_expired && web_login::is_web(&self.settings) {
                section.retry = if gate.blocked_until > context.now {
                    format!("更新登录后仍需等待至 {}", crate::timefmt::clock(gate.blocked_until, context.now, context.utc_offset))
                } else { "已暂停请求，更新登录后恢复".into() };
            }
            if section.unsubscribed {
                // Empty while a recheck may be asked for; otherwise when the asked one runs.
                let at = gate.blocked_until.max(gate.last_attempt.saturating_add(schedule::MIN_GAP_SECS));
                section.retry = if self.unpaid[section.provider.index()] != 0 {
                    String::new()
                } else if at > context.now && !self.fetching.as_ref().is_some_and(|(p, _)| *p == section.provider) {
                    format!("将于 {} 重新检查", crate::timefmt::clock(at, context.now, context.utc_offset))
                } else {
                    "正在重新检查…".into()
                };
            }
        }
        sections
    }

    /// Redraws the panel's surface if its content has changed.
    unsafe fn refresh_popup(&mut self) {
        if self.popup.is_none() {
            return;
        }
        let content = (self.panel(), self.light);
        let Some(popup) = self.popup.as_mut() else {
            return;
        };
        if popup.shown.as_ref() == Some(&content) {
            return;
        }
        let (w, h) = popup::size(popup.scale, &content.0);
        if h != popup.surface.h {
            // A section changed between bars and a notice: keep the bottom edge in place.
            let (Some(surface), Some(scratch)) = (Surface::new(w, h), Surface::new(w, h)) else {
                return;
            };
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetWindowRect(popup.hwnd, &mut rect);
            SetWindowPos(popup.hwnd, null_mut(), rect.left, rect.bottom - h, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            popup.surface = surface;
            popup.scratch = scratch;
        }
        popup::paint(&mut popup.surface, &mut popup.scratch, &content.0, content.1, popup.scale);
        popup.shown = Some(content);
        InvalidateRect(popup.hwnd, null(), 0);
    }

    unsafe fn blit_popup(&mut self, hwnd: HWND) {
        let mut paint: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut paint);
        if let Some(popup) = self.popup.as_ref().filter(|popup| popup.hwnd == hwnd) {
            BitBlt(dc, 0, 0, popup.surface.w, popup.surface.h, popup.surface.dc, 0, 0, SRCCOPY);
        }
        EndPaint(hwnd, &paint);
    }

    /// Creates the widget if needed and keeps it sized and placed against the tray.
    /// Leaves `widget` null when the taskbar cannot host it; the tray icon still works.
    unsafe fn ensure_widget(&mut self) {
        if self.drag.is_some() && !self.widget.is_null() && IsWindow(self.widget) != 0 {
            return;
        }
        self.drag = None;
        let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        let tray = if taskbar.is_null() {
            null_mut()
        } else {
            FindWindowExW(taskbar, null_mut(), wide("TrayNotifyWnd").as_ptr(), null())
        };
        let mut bar = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut notify = bar;
        if tray.is_null() || GetWindowRect(taskbar, &mut bar) == 0 || GetWindowRect(tray, &mut notify) == 0 {
            self.drop_widget();
            return;
        }

        let dpi = GetDpiForWindow(taskbar);
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let w = (WIDTH * scale).round() as i32;
        let h = bar.bottom - bar.top;
        let anchor = (notify.left - bar.left - w).max(0);
        let x = (anchor - (self.settings.offset_x as f32 * scale).round() as i32).clamp(0, anchor);
        let geom = Geom { x, w, h, anchor, scale };

        if self.widget.is_null() || IsWindow(self.widget) == 0 || GetParent(self.widget) != taskbar {
            self.drop_widget();
            self.widget = CreateWindowExW(
                WS_EX_LAYERED,
                wide("IUsageWidget").as_ptr(),
                null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                x,
                0,
                w,
                h,
                taskbar,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if self.widget.is_null() {
                return;
            }
            self.geom = Geom::default();
        }
        if geom != self.geom {
            if (geom.w, geom.h) != (self.geom.w, self.geom.h) {
                let (Some(surface), Some(scratch)) = (Surface::new(w, h), Surface::new(w, h)) else {
                    self.drop_widget();
                    return;
                };
                self.surface = Some(surface);
                self.scratch = Some(scratch);
            }
            SetWindowPos(self.widget, null_mut(), x, 0, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            self.geom = geom;
            self.shown = None;
        }
    }

    unsafe fn drop_widget(&mut self) {
        if !self.widget.is_null() && IsWindow(self.widget) != 0 {
            DestroyWindow(self.widget);
        }
        self.widget = null_mut();
        self.geom = Geom::default();
        self.surface = None;
        self.scratch = None;
        self.drag = None;
    }

    /// Repaints the widget, the tray icon and the detail panel if what they show has changed.
    unsafe fn refresh(&mut self) {
        self.refresh_popup();
        let now = unix_now();
        let utc_offset = utc_offset_secs();
        let provider = self.settings.provider;
        let i = provider.index();
        let mut view = view::build(
            provider,
            self.cache[i].as_ref().map(|entry| &entry.usage),
            self.error[i].as_ref(),
            self.settings.show_remaining,
            self.settings.thresholds,
            now,
        );
        if let Some(entry) = self.cache[i] {
            view.tip.push_str(&format!("\n{} · 上次成功 {}", entry.source.label(), crate::timefmt::clock(entry.fetched_at, now, utc_offset)));
        }
        if schedule::told_to_wait(self.schedule[i].blocked_until, now) {
            view.tip.push_str(&format!("\n等待至 {}", crate::timefmt::clock(self.schedule[i].blocked_until, now, utc_offset)));
            view.stale |= self.cache[i].is_some() && self.schedule[i].backoff > 0;
        }
        let shown = Shown { view, light: self.light };
        if self.shown.as_ref() == Some(&shown) {
            return;
        }
        if !self.shown.as_ref().is_some_and(|old| old.light == shown.light && old.view.same_pixels(&shown.view)) {
            self.paint(&shown);
        }
        self.shown = Some(shown);
        self.set_tray(NIM_MODIFY);
    }

    unsafe fn paint(&mut self, shown: &Shown) {
        if self.widget.is_null() {
            return;
        }
        let (Some(surface), Some(scratch)) = (self.surface.as_mut(), self.scratch.as_mut()) else {
            return;
        };
        let s = self.geom.scale;
        let (w, h) = (surface.w, surface.h);
        let fade = if shown.view.stale { STALE_FADE } else { 1.0 };
        let (text, track) = theme_colors(shown.light);
        let text = text.faded(fade);

        let bar_h = (BAR_H * s).round();
        let gap = (ROW_GAP * s).round();
        let top = ((h as f32 - bar_h * 2.0 - gap) / 2.0).floor();
        let row_y = [top, top + bar_h + gap];

        // Alpha 1 everywhere: invisible, but the whole rectangle receives mouse input.
        surface.pixels().fill(0x0100_0000);
        {
            let mut canvas = surface.canvas();
            let middle = h as f32 / 2.0;
            for column in [2.0, 6.0] {
                for dy in [-7.5, -2.5, 2.5, 7.5] {
                    canvas.circle((PAD_L + column) * s, middle + dy * s, 0.85 * s, text.faded(0.45));
                }
            }
            for (row, y) in shown.view.rows.iter().zip(row_y) {
                let track = if row.fill.is_some() { track } else { track.faded(0.5) };
                canvas.round_rect(BAR_X * s, y, BAR_W * s, bar_h, BAR_RADIUS * s, f32::MAX, track);
                if let Some(fill) = row.fill.filter(|f| *f > 0.0) {
                    let right = (BAR_X + BAR_W * fill) * s;
                    let color = fill_color(row.severity).faded(fade);
                    canvas.round_rect(BAR_X * s, y, BAR_W * s, bar_h, BAR_RADIUS * s, right, color);
                }
            }
        }

        let regular = Font::new((FONT_PX * s).round() as i32, false);
        let bold = Font::new((FONT_PX * s).round() as i32, true);
        let badge_font = Font::new((BADGE_FONT_PX * s).round() as i32, true);
        let rect = |x: f32, width: f32, top: f32, bottom: f32| RECT {
            left: (x * s).round() as i32,
            top: top as i32,
            right: ((x + width) * s).round() as i32,
            bottom: bottom as i32,
        };
        let (letter, badge_color) = badge(shown.view.provider);
        draw_text(surface, scratch, &badge_font, letter, rect(BADGE_X, BADGE_W, 0.0, h as f32), Align::Center, badge_color);
        for (row, y) in shown.view.rows.iter().zip(row_y) {
            // A band taller than the bar, sharing its centre, so glyphs are not clipped.
            let (band_top, band_bottom) = (y - bar_h, y + bar_h * 2.0);
            let label = if row.fill.is_some() { text } else { text.faded(0.5) };
            draw_text(surface, scratch, &bold, row.label, rect(LABEL_X, LABEL_W, band_top, band_bottom), Align::Left, label);
            draw_text(surface, scratch, &regular, &row.text, rect(TEXT_X, TEXT_W, band_top, band_bottom), Align::Left, text);
        }

        let size = SIZE { cx: w, cy: h };
        let origin = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
        // No destination point: the position stays whatever SetWindowPos made it.
        UpdateLayeredWindow(self.widget, null_mut(), null(), &size, surface.dc, &origin, 0, &blend, ULW_ALPHA);
    }

    /// A notify-icon record that identifies our tray icon and nothing else.
    fn tray_data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.main;
        data.uID = TRAY_ID;
        data
    }

    unsafe fn set_tray(&mut self, message: NOTIFY_ICON_MESSAGE) {
        if self.icon.is_null() {
            self.icon = load_app_icon();
        }
        let mut data = self.tray_data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icon;
        let tip = self.shown.as_ref().map_or("IUsage", |shown| shown.view.tip.as_str());
        copy_wide(&mut data.szTip, tip);
        Shell_NotifyIconW(message, &data);
    }

    /// Raises a system notification through the tray icon.
    unsafe fn notify(&self, provider: Provider, alert: &Alert) {
        let mut data = self.tray_data();
        data.uFlags = NIF_INFO;
        copy_wide(&mut data.szInfoTitle, &format!("{} 用量提醒", provider.name()));
        copy_wide(&mut data.szInfo, &alert.lines.join("\n"));
        data.dwInfoFlags = if alert.critical { NIIF_WARNING } else { NIIF_INFO };
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }

    unsafe fn shutdown(&mut self) {
        let data = self.tray_data();
        Shell_NotifyIconW(NIM_DELETE, &data);
        self.close_popup();
        self.drop_widget();
        if !self.icon.is_null() {
            DestroyIcon(self.icon);
            self.icon = null_mut();
        }
        // Saves already asked for are written before the process ends. A request worker
        // that persists after this gets an error and sends nothing.
        if let Some((saver, writer)) = self.saver.take() {
            let _ = saver.send(Save::Stop);
            let _ = writer.join();
        }
    }

    fn start_fetch(&mut self, provider: Provider) {
        if provider == Provider::Claude && self.web_expired && web_login::is_web(&self.settings) { return; }
        let now = unix_now();
        let i = provider.index();
        if self.fetching.is_some() || !self.schedule[i].can_request(now) {
            return;
        }
        self.schedule[i].started(now);
        let stored = self.stored();
        let cache_path = self.cache_path.clone();
        let saver = self.saver.as_ref().map(|(saver, _)| saver.clone());
        let env = Arc::clone(&self.env);
        let settings = self.settings.clone();
        let main = self.main as usize;
        let spawned = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            // Persist before any HTTP call, off the shared Explorer/UI thread, once even
            // when a token renewal comes before the usage request. Missing credentials
            // and ones that cannot be renewed need no disk writes and no HTTP.
            let persisted = std::cell::Cell::new(false);
            let persist = || -> Result<(), FetchError> {
                if !persisted.replace(true) {
                    // Through the writer, so an earlier save still queued there cannot
                    // land after this one and take the cooldown off the disk.
                    let saved = match &saver {
                        Some(saver) => {
                            let (ack, done) = mpsc::sync_channel(1);
                            saver.send(Save::Write(stored, Some(ack))).map_err(|_| "cache writer stopped".to_string())
                                .and_then(|()| done.recv().map_err(|_| "cache writer stopped".to_string()))
                                .and_then(|result| result.map_err(|e| e.to_string()))
                        }
                        None => cache::save(&cache_path, &stored).map_err(|e| e.to_string()),
                    };
                    saved.map_err(|e| FetchError::Network(format!("Cannot persist request cooldown: {e}")))?;
                }
                Ok(())
            };
            let session = if provider == Provider::Claude { web_login::selected(&settings) } else { Ok(None) };
            let result = session.and_then(|session| providers::fetch_selected(provider, &env, unix_now(), http::send, session.as_ref(), &mut || persist()));
            unsafe { PostMessageW(main as HWND, WM_APP_FETCHED, 0, 0) };
            result
        });
        match spawned {
            Ok(worker) => self.fetching = Some((provider, worker)),
            Err(error) => {
                let error = FetchError::Network(format!("Cannot start worker: {error}"));
                self.schedule[i].failed(&error, now);
                self.error[i] = Some(error);
                self.save_cache();
            }
        }
    }

    unsafe fn on_fetched(&mut self) {
        if !self.fetching.as_ref().is_some_and(|(_, worker)| worker.is_finished()) {
            return;
        }
        let (provider, worker) = self.fetching.take().unwrap();
        let result = worker.join().unwrap_or_else(|_| Err(FetchError::Network("Worker stopped unexpectedly".into())));
        let now = unix_now();
        let i = provider.index();
        if provider == Provider::Claude && self.discard_claude_result {
            self.discard_claude_result = false;
            match result {
                Ok(_) | Err(FetchError::Unsubscribed) => self.schedule[i].succeeded(now, self.settings.poll_secs),
                Err(ref error) => self.schedule[i].failed(error, now),
            }
            self.save_cache();
            self.refresh();
            return;
        }
        match result {
            Ok((usage, source)) => {
                // While notifications are off nothing counts as announced, so turning
                // them on reports the current state at the next reading.
                let mut announced = [0, 0];
                let mut alert = None;
                if self.settings.notify {
                    announced = self.cache[i].map_or(announced, |entry| entry.announced);
                    if let Some(previous) = self.cache[i] {
                        alert::rearm(&mut announced, &previous.usage, &usage, now);
                    }
                    if provider == self.settings.provider {
                        alert = alert::check(&mut announced, &usage, now);
                    }
                }
                self.cache[i] = Some(Entry { usage, source, fetched_at: now, announced });
                if provider == Provider::Claude && source == cache::Source::Web { self.web_expired = false; }
                self.unpaid[i] = 0;
                self.error[i] = None;
                self.schedule[i].succeeded(now, self.settings.poll_secs);
                self.save_cache();
                if let Some(alert) = alert {
                    self.notify(provider, &alert);
                }
            }
            Err(FetchError::Unsubscribed) => {
                // The service answered normally: no backoff, nothing for the error file,
                // and the reading from when the account was paid no longer applies.
                self.schedule[i].succeeded(now, self.settings.poll_secs);
                self.unpaid[i] = now;
                self.cache[i] = None;
                self.error[i] = Some(FetchError::Unsubscribed);
                self.save_cache();
            }
            Err(error) => {
                self.schedule[i].failed(&error, now);
                let login_notice = provider == Provider::Claude && cache::mark_web_expired(&mut self.web_expired, &error);
                // One line for troubleshooting; the file's own timestamp says when.
                // An unchanged missing credential should not rewrite the disk every 15s.
                if self.error[i].as_ref() != Some(&error) {
                    let _ = std::fs::write(&self.error_path, format!("{}: {error:?}\n", provider.name()));
                }
                let local = matches!(error, FetchError::NoCredentials | FetchError::Expired | FetchError::Offline(_));
                self.error[i] = Some(error);
                if !local {
                    self.save_cache();
                }
                if login_notice {
                    let mut data = self.tray_data();
                    data.uFlags = NIF_INFO;
                    copy_wide(&mut data.szInfoTitle, "Claude 网页登录已失效");
                    copy_wide(&mut data.szInfo, "已保留上次额度并暂停请求。点击此通知更新登录信息，也可从右键菜单进入。");
                    data.dwInfoFlags = NIIF_WARNING;
                    Shell_NotifyIconW(NIM_MODIFY, &data);
                }
            }
        }
        // The user may have switched while this request was in flight.
        self.fetch_if_due();
        self.refresh();
    }

    unsafe fn on_command(&mut self, command: u32) {
        match command {
            CMD_WEB_CONFIG => self.open_web_config(),
            CMD_CLAUDE => self.set_provider(Provider::Claude),
            CMD_CODEX => self.set_provider(Provider::Codex),
            CMD_USED | CMD_REMAINING => {
                self.settings.show_remaining = command == CMD_REMAINING;
                self.save_settings();
                self.refresh();
            }
            CMD_NOTIFY => {
                self.settings.notify = !self.settings.notify;
                self.save_settings();
            }
            CMD_AUTOSTART => {
                // The registry is the only state: the menu reads it back next time it opens.
                autostart::set_enabled(!autostart::is_enabled());
            }
            _ => {}
        }
    }

    fn open_web_config(&self) {
        let gate = self.schedule[Provider::Claude.index()];
        let waiting = if gate.blocked_until > unix_now() {
            format!("原等待截止：{}。", crate::timefmt::clock(gate.blocked_until, unix_now(), unsafe { utc_offset_secs() }))
        } else { "已有缓存和刷新间隔仍然有效。".into() };
        web_login::open(self.main, waiting, self.web_expired);
    }

    unsafe fn set_claude_source(&mut self, web: bool) {
        let mut settings = self.settings.clone();
        settings.claude_web = Some(web);
        if settings.save(&self.settings_path).is_err() {
            let mut data = self.tray_data();
            data.uFlags = NIF_INFO;
            copy_wide(&mut data.szInfoTitle, "无法切换 Claude 数据来源");
            copy_wide(&mut data.szInfo, "渠道设置无法保存，请检查 IUsage 设置目录的写入权限后重试。");
            data.dwInfoFlags = NIIF_WARNING;
            Shell_NotifyIconW(NIM_MODIFY, &data);
            return;
        }
        self.settings = settings;
        self.web_expired = false;
        self.unpaid[0] = 0;
        // Credentials may refer to a different account. Never label the old result as new.
        self.cache[0] = None;
        self.error[0] = None;
        self.discard_claude_result = self.fetching.as_ref().is_some_and(|(p, _)| *p == Provider::Claude);
        self.save_cache();
        self.refresh();
        // The normal timer applies the unchanged persisted gate and poll schedule.
    }

    unsafe fn set_provider(&mut self, provider: Provider) {
        if provider == self.settings.provider {
            return;
        }
        self.settings.provider = provider;
        self.save_settings();
        // Switching shows the last reading for that provider and only asks the
        // service if that reading has aged out.
        self.fetch_if_due();
        self.refresh();
    }

    unsafe fn on_down(&mut self, client_x: i32) {
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        SetCapture(self.widget);
        self.drag = Some(Drag {
            grip: (client_x as f32) < (PAD_L + GRIP_W + 2.0) * self.geom.scale,
            moved: false,
            start_cursor: cursor.x,
            start_x: self.geom.x,
        });
    }

    unsafe fn on_move(&mut self) {
        let Some(drag) = self.drag.as_mut().filter(|drag| drag.grip) else {
            return;
        };
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        let dx = cursor.x - drag.start_cursor;
        if dx.abs() > 3 {
            drag.moved = true;
        }
        if !drag.moved {
            return;
        }
        let x = (drag.start_x + dx).clamp(0, self.geom.anchor);
        if x != self.geom.x {
            self.geom.x = x;
            SetWindowPos(self.widget, null_mut(), x, 0, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    unsafe fn on_up(&mut self, client_x: i32) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        ReleaseCapture();
        if drag.grip {
            if drag.moved {
                self.commit_offset();
            }
            return;
        }
        // A click on the badge switches between the two assistants; anywhere else
        // opens or closes the detail panel.
        let x = client_x as f32 / self.geom.scale;
        if (BADGE_X - 2.0..BADGE_X + BADGE_W + 2.0).contains(&x) {
            self.set_provider(self.settings.provider.other());
        } else {
            self.toggle_popup();
        }
    }

    /// Capture was taken away mid-drag (Alt-Tab, UAC prompt): keep the position, never click.
    fn on_capture_lost(&mut self) {
        if let Some(drag) = self.drag.take() {
            if drag.grip && drag.moved {
                self.commit_offset();
            }
        }
    }

    fn commit_offset(&mut self) {
        self.settings.offset_x = ((self.geom.anchor - self.geom.x) as f32 / self.geom.scale).round() as i32;
        self.save_settings();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_writer_keeps_order_and_acknowledges_after_writing() {
        let path = std::env::temp_dir().join(format!("iusage-saver-{}.json", std::process::id()));
        let (saver, writer) = start_saver(path.clone()).unwrap();
        let mut stored = Stored::default();
        for n in 1..=20 {
            stored.schedule[0].last_attempt = n;
            saver.send(Save::Write(stored, None)).unwrap();
        }
        stored.schedule[1].blocked_until = 99;
        let (ack, done) = mpsc::sync_channel(1);
        saver.send(Save::Write(stored, Some(ack))).unwrap();
        done.recv().unwrap().unwrap();
        // Acknowledged means on disk, with nothing older written after it.
        assert_eq!(cache::load(&path).schedule, stored.schedule);
        saver.send(Save::Stop).unwrap();
        writer.join().unwrap();
        assert!(saver.send(Save::Write(stored, None)).is_err(), "a stopped writer refuses, so nothing is sent unsaved");
        std::fs::remove_file(path).unwrap();
    }
}
