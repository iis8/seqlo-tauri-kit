//! 启动页（splash）框架：双窗口（splash + main）的启动汇合、macOS alpha 握手、原生主题同步。
//!
//! ## 架构
//!
//! 双窗口架构：splash（无边框，加载独立的 `splash.html`）+ main（主窗口）。两者都以
//! `visible: false` 在配置中创建，必须由 Rust 侧程序化 show；splash 的 show 必须走
//! 「先透明、后显形」的 alpha 握手（见 [`SplashController::on_splash_painted`]）。
//!
//! ## 硬约束（搬运自桌面端 AGENTS.md「启动页不可变形约束」）
//!
//! 1. main / splash 都 `visible: false`；splash 由 `on_page_load` 触发、以 `alpha=0` 上屏，
//!    首帧后再恢复 `alpha=1`。
//! 2. splash 关闭必须 STORAGE_READY ∧ FRONTEND_READY 双汇合（[`SplashController`] 内部保证）。
//! 3. `SPLASH_MIN_DURATION` 不能删不能缩短（默认 900ms）。
//! 4. 三道兜底不得删：5s show 兜底、1.5s paint 兜底、20s watchdog（watchdog 由宿主在 setup 里起）。
//!
//! ## 用法
//!
//! ```ignore
//! // setup 里：
//! app.manage(vc_desktop_kit::splash::SplashController::with_defaults());
//! // on_page_load 回调：vc_desktop_kit::splash::on_splash_page_loaded(app);
//! // generate_handler! 里加入：cmd_splash_painted / cmd_frontend_ready / cmd_set_window_theme
//! // 存储就绪后：app.state::<SplashController>().mark_storage_ready(&app)
//! ```

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, WebviewWindow};

/// 窗口标签：splash 与主窗口。默认分别为 `"splash"` / `"main"`，可自定义。
#[derive(Clone, Copy)]
pub struct WindowLabels {
    pub splash: &'static str,
    pub main: &'static str,
}

impl WindowLabels {
    pub const DEFAULT: WindowLabels = WindowLabels {
        splash: "splash",
        main: "main",
    };
}

/// 启动页相关时限。
#[derive(Clone, Copy)]
pub struct SplashTimings {
    /// splash 最短展示时长：本地库初始化很快时，splash 也至少播完一轮入场动画。
    pub min_duration: Duration,
    /// splash show 兜底时限：万一页面加载完成回调不触发，也要在有限时间内 show。
    pub show_fallback: Duration,
    /// splash 首帧兜底时限：透明窗口 show 后页面迟迟不回调时，强制恢复不透明度。
    pub paint_fallback: Duration,
}

impl Default for SplashTimings {
    fn default() -> Self {
        SplashTimings {
            min_duration: Duration::from_millis(900),
            show_fallback: Duration::from_secs(5),
            paint_fallback: Duration::from_millis(1500),
        }
    }
}

const THEME_UNKNOWN: u8 = 0;
const THEME_LIGHT: u8 = 1;
const THEME_DARK: u8 = 2;

/// 启动页控制器：取代桌面端 lib.rs 里的模块级 static，封装全部启动汇合状态机。
///
/// 通过 `app.manage(SplashController::…)` 注册为托管状态；命令与宿主回调经由
/// `tauri::State<SplashController>` 访问。所有方法都可安全并发调用（内部用原子量保证只触发一次）。
///
/// 注意：内部需要跨线程操作窗口的逻辑，统一通过 `app.try_state::<SplashController>()`
/// 在目标线程重新取回 `&self`，避免把 `&self` 捕获进 `'static` 线程闭包。
pub struct SplashController {
    labels: WindowLabels,
    timings: SplashTimings,
    storage_ready: AtomicBool,
    frontend_ready: AtomicBool,
    splash_done: AtomicBool,
    splash_shown: AtomicBool,
    splash_painted: AtomicBool,
    splash_shown_at: OnceLock<Instant>,
    app_theme: AtomicU8,
}

impl SplashController {
    /// 用自定义窗口标签与时限构造。
    pub fn new(labels: WindowLabels, timings: SplashTimings) -> Self {
        SplashController {
            labels,
            timings,
            storage_ready: AtomicBool::new(false),
            frontend_ready: AtomicBool::new(false),
            splash_done: AtomicBool::new(false),
            splash_shown: AtomicBool::new(false),
            splash_painted: AtomicBool::new(false),
            splash_shown_at: OnceLock::new(),
            app_theme: AtomicU8::new(THEME_UNKNOWN),
        }
    }

    /// 默认标签（splash/main）与默认时限。
    pub fn with_defaults() -> Self {
        Self::new(WindowLabels::DEFAULT, SplashTimings::default())
    }

    fn theme_code(theme: &str) -> Option<u8> {
        match theme {
            "light" => Some(THEME_LIGHT),
            "dark" => Some(THEME_DARK),
            _ => None,
        }
    }

    fn theme_from_code(code: u8) -> Option<tauri::Theme> {
        match code {
            THEME_LIGHT => Some(tauri::Theme::Light),
            THEME_DARK => Some(tauri::Theme::Dark),
            _ => None,
        }
    }
}

// ============ 内部跨线程编排（不捕获 &self，统一经托管状态取回） ============

/// 设置 splash 原生窗口不透明度（macOS 公开 API）。
#[cfg(target_os = "macos")]
fn set_splash_alpha(win: &WebviewWindow, alpha: f64) {
    let Ok(ns_window) = win.ns_window() else {
        eprintln!("[SPLASH] 取 ns_window 失败，透明度未设置 alpha={alpha}");
        return;
    };
    let obj = ns_window as *mut objc2::runtime::AnyObject;
    unsafe {
        let _: () = objc2::msg_send![obj, setAlphaValue: alpha];
    }
}

/// 把 `app_theme` 写到原生窗口外观，应用级 + 窗口级都调一次。失败时仅打印一行。
fn apply_native_theme(app: &AppHandle, stage: &str) {
    let Some(ctrl) = app.try_state::<SplashController>() else {
        eprintln!("[WINDOW-THEME] {stage}: 控制器未注册，跳过原生外观同步");
        return;
    };
    let Some(theme) = SplashController::theme_from_code(ctrl.app_theme.load(Ordering::SeqCst)) else {
        eprintln!("[WINDOW-THEME] {stage}: 应用主题未上报，原生外观保持跟随系统");
        return;
    };
    app.set_theme(Some(theme));

    let Some(win) = app.get_webview_window(ctrl.labels.main) else {
        eprintln!("[WINDOW-THEME] {stage}: theme={theme:?} app=ok window=main 不存在");
        return;
    };
    let visible = win.is_visible().unwrap_or(false);
    let window = match win.set_theme(Some(theme)) {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("Err({e})"),
    };
    eprintln!(
        "[WINDOW-THEME] {stage}: theme={theme:?} app=ok window={window} visible={visible}"
    );
}

/// show splash。必须等 splash.html 首屏加载完成后再调用（见 `on_splash_page_loaded`）。
/// 窗口操作统一投递到主线程。保证只 show 一次（与兜底共用）。
fn show_splash_once(app: &AppHandle) {
    let Some(ctrl) = app.try_state::<SplashController>() else {
        eprintln!("[SPLASH] 控制器未注册，无法 show");
        return;
    };
    if ctrl.splash_shown.swap(true, Ordering::SeqCst) {
        return;
    }
    let labels = ctrl.labels;
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(splash) = handle.get_webview_window(labels.splash) else {
            return;
        };
        #[cfg(target_os = "macos")]
        set_splash_alpha(&splash, 0.0);
        if let Some(ctrl) = handle.try_state::<SplashController>() {
            let _ = ctrl.splash_shown_at.set(Instant::now());
        }
        let _ = splash.show();
        // splash 已 show：若存储与前端都已就绪，直接进入收尾。
        finish_if_ready(&handle);
        // 页面首帧迟迟不来也不能一直透明：超时后强制显形。
        let handle_for_alpha = handle.clone();
        let paint_fallback = labels_and_paint_fallback(&handle_for_alpha);
        std::thread::spawn(move || {
            std::thread::sleep(paint_fallback);
            let Some(ctrl) = handle_for_alpha.try_state::<SplashController>() else {
                return;
            };
            if ctrl.splash_painted.load(Ordering::SeqCst)
                || !ctrl.splash_shown.load(Ordering::SeqCst)
            {
                return;
            }
            let inner = handle_for_alpha.clone();
            let _ = handle_for_alpha.run_on_main_thread(move || {
                if let Some(win) = inner.get_webview_window(labels.splash) {
                    #[cfg(target_os = "macos")]
                    set_splash_alpha(&win, 1.0);
                    let _ = win.set_focus();
                }
            });
        });
    });
}

/// 从托管状态取 splash 标签与 paint 兜底（供线程内使用）。
fn labels_and_paint_fallback(app: &AppHandle) -> Duration {
    match app.try_state::<SplashController>() {
        Some(ctrl) => ctrl.timings.paint_fallback,
        None => Duration::from_millis(1500),
    }
}

/// 关闭 splash、显示主窗口。仅在「存储就绪 && 前端就绪 && splash 已 show」时执行，且全程只执行一次。
/// 必须在主线程操作窗口，避免后台线程直接碰窗口的平台限制。
fn finish_if_ready(app: &AppHandle) {
    let Some(ctrl) = app.try_state::<SplashController>() else {
        return;
    };
    if !ctrl.storage_ready.load(Ordering::SeqCst) || !ctrl.frontend_ready.load(Ordering::SeqCst) {
        return;
    }
    // splash 还没 show：先触发 show，等 splash show 完成后再回来收尾。
    if !ctrl.splash_shown.load(Ordering::SeqCst) {
        show_splash_once(app);
        return;
    }
    if ctrl.splash_done.swap(true, Ordering::SeqCst) {
        return;
    }

    let labels = ctrl.labels;
    let min_duration = ctrl.timings.min_duration;
    let elapsed = ctrl
        .splash_shown_at
        .get()
        .map(|t| t.elapsed())
        .unwrap_or(min_duration);
    let wait = min_duration.saturating_sub(elapsed);
    let handle = app.clone();
    std::thread::spawn(move || {
        if wait > Duration::ZERO {
            std::thread::sleep(wait);
        }
        let h = handle.clone();
        let h2 = h.clone();
        let _ = h.run_on_main_thread(move || {
            if let Some(win) = handle.get_webview_window(labels.main) {
                apply_native_theme(&handle, "before-show");
                let _ = win.show();
                let _ = win.set_focus();
                apply_native_theme(&handle, "after-show");
            }
            if let Some(splash) = h2.get_webview_window(labels.splash) {
                let _ = splash.close();
            }
        });
    });
}

// ============ 对外 API（命令 / 宿主回调） ============

impl SplashController {
    /// 存储就绪：建库 + 迁移完成后调用。与前端就绪汇合后关闭 splash。
    pub fn mark_storage_ready(&self, app: &AppHandle) {
        self.storage_ready.store(true, Ordering::SeqCst);
        finish_if_ready(app);
    }

    /// 前端首屏挂载完成（main.js 在 mount 后 invoke `frontend_ready`）。
    /// `theme` 是应用内当前主题（light/dark）：只记录，不在此处切换原生外观。
    pub fn on_frontend_ready(&self, app: AppHandle, theme: Option<&str>) {
        eprintln!("[WINDOW-THEME] frontend_ready 收到 theme={theme:?}");
        if let Some(code) = theme.and_then(Self::theme_code) {
            self.app_theme.store(code, Ordering::SeqCst);
        } else {
            eprintln!("[WINDOW-THEME] frontend_ready 未收到合法主题，原生外观将跟随系统");
        }
        self.frontend_ready.store(true, Ordering::SeqCst);
        finish_if_ready(&app);
    }

    /// splash.html 首帧回调：页面画完第一帧后把透明窗口显形。
    pub fn on_splash_painted(&self, app: AppHandle) {
        if self.splash_painted.swap(true, Ordering::SeqCst) {
            return;
        }
        let labels = self.labels;
        let _ = app.clone().run_on_main_thread(move || {
            if let Some(win) = app.get_webview_window(labels.splash) {
                #[cfg(target_os = "macos")]
                set_splash_alpha(&win, 1.0);
                let _ = win.set_focus();
            }
        });
    }

    /// splash 页面加载完成回调（`Builder::on_page_load` 的 `PageLoadEvent::Finished`）。
    /// 由宿主在 `on_page_load` 里判断 label == splash 后调用。
    pub fn on_splash_page_loaded(&self, app: AppHandle) {
        show_splash_once(&app);
    }

    /// 5s show 兜底：万一页面加载完成回调不触发（资源缺失/导航失败），也仍走 show。
    pub fn show_fallback(&self, app: &AppHandle) {
        show_splash_once(app);
    }

    /// 20s watchdog：万一前端就绪事件丢失，强制进入主窗口。
    pub fn force_finish(&self, app: &AppHandle) {
        if self.splash_done.load(Ordering::SeqCst) {
            return;
        }
        self.storage_ready.store(true, Ordering::SeqCst);
        self.frontend_ready.store(true, Ordering::SeqCst);
        finish_if_ready(app);
    }

    /// 应用内主题切换时同步原生窗口外观（标题栏文字/底色、系统控件配色）。
    /// 仅桌面端前端调用；Web 环境无此能力。
    pub fn set_theme(&self, app: AppHandle, theme: &str) -> Result<String, String> {
        let code = Self::theme_code(theme).ok_or_else(|| {
            eprintln!("[WINDOW-THEME] set_window_theme 收到非法主题: {theme}");
            format!("未知主题: {theme}")
        })?;
        self.app_theme.store(code, Ordering::SeqCst);
        let target = Self::theme_from_code(code).ok_or_else(|| format!("未知主题: {theme}"))?;
        app.set_theme(Some(target));

        let Some(win) = app.get_webview_window(self.labels.main) else {
            let msg = "main 窗口不存在".to_string();
            eprintln!("[WINDOW-THEME] set_window_theme: {msg}");
            return Err(msg);
        };
        let visible = win.is_visible().unwrap_or(false);
        match win.set_theme(Some(target)) {
            Ok(()) => {
                let readback = win
                    .theme()
                    .map(|t| format!("{t:?}"))
                    .unwrap_or_else(|e| format!("读取失败: {e}"));
                eprintln!(
                    "[WINDOW-THEME] set_window_theme: theme={target:?} app=ok window=ok visible={visible} readback={readback}"
                );
                Ok(format!(
                    "ok theme={target:?} visible={visible} readback={readback}"
                ))
            }
            Err(e) => {
                eprintln!(
                    "[WINDOW-THEME] set_window_theme: theme={target:?} app=ok window=Err({e}) visible={visible}"
                );
                Err(format!("set_theme 失败（target={target:?}）: {e}"))
            }
        }
    }
}

// ============ Tauri 命令（宿主在 generate_handler! 中注册） ============

/// splash.html 首帧回调命令。宿主在 `generate_handler!` 中登记：
/// `vc_desktop_kit::splash::cmd_splash_painted`。
#[tauri::command]
pub fn cmd_splash_painted(app: AppHandle, ctrl: tauri::State<SplashController>) {
    ctrl.on_splash_painted(app);
}

/// 前端首屏就绪命令。`theme` 由前端传应用内主题（light/dark）。
#[tauri::command]
pub fn cmd_frontend_ready(
    app: AppHandle,
    theme: Option<String>,
    ctrl: tauri::State<SplashController>,
) {
    ctrl.on_frontend_ready(app, theme.as_deref());
}

/// 应用内主题 → 原生窗口外观命令。
#[tauri::command]
pub fn cmd_set_window_theme(
    app: AppHandle,
    theme: String,
    ctrl: tauri::State<SplashController>,
) -> Result<String, String> {
    ctrl.set_theme(app, &theme)
}
