//! 系统托盘（Windows / macOS）与「关闭按钮行为」的通用实现，从桌面端 tray.rs 抽取。
//!
//! ## 配置（`kv["window"]`，默认值只活在代码里）
//!
//! - `tray_enabled`：是否启用系统托盘图标，默认 `true`
//! - `close_confirm`：关窗时是否弹「收进托盘 / 隐藏 / 退出」确认窗，默认 `true`
//! - `close_action`：`"tray"` | `"hide"` | `"quit"`；**缺省按平台默认**
//!   （macOS → `hide`，Windows → `quit`）。`"hide"` 只在 macOS 有意义，
//!   Windows 读到 `"hide"` 安全回落 `"quit"`。
//!
//! ## 行为矩阵
//!
//! | 托盘 | 平台 | close_confirm | close_action | 点关闭按钮 |
//! |---|---|---|---|---|
//! | 开 | 任意 | 任意 | 不参与 | 阻止关闭 → 隐藏窗口（macOS Accessory 收 Dock / Windows skip_taskbar） |
//! | 关 | macOS | true | 任意 | 弹确认窗（三选），默认「隐藏窗口」 |
//! | 关 | macOS | false | 缺省 / `hide` | 隐藏窗口，保持 Regular（Dock 在，点 Dock 经 Reopen 恢复） |
//! | 关 | macOS | false | `quit` | 直接退出 |
//! | 关 | macOS | false | `tray` | 开启托盘并收进托盘 |
//! | 关 | Windows | true | 任意 | 弹确认窗（两选），默认「退出程序」 |
//! | 关 | Windows | false | 缺省 / `quit` / `hide`(回落) | 直接退出 |
//! | 关 | Windows | false | `tray` | 开启托盘并收进托盘 |
//! | 任意 | 任意 | 任意 | 任意 | 退出流程（托盘菜单退出 / Cmd+Q / Dock 退出 / 注销）→ 一律放行 |
//!
//! 决策全部由纯函数 [`resolve_close_decision`] / [`resolve_dialog_decision`] 给出，
//! [`WindowController::on_close_requested`] 只负责执行。
//!
//! ## 平台差异
//!
//! - macOS：收进托盘时 `set_activation_policy(Accessory)`（Dock 图标消失，只剩托盘）；
//!   保留 Dock 的隐藏用 `Regular` + `win.hide()`，点 Dock 图标经 `RunEvent::Reopen` 恢复。
//!   托盘图标用宿主注入的 PNG 模板图（22×22 单色 + alpha，系统按 alpha 适配深浅菜单栏）。
//! - Windows：收进托盘时 `set_skip_taskbar(true)`；托盘图标用应用图标（彩色）。

use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
pub use tauri::CloseRequestApi;

use crate::config::ConfigSection;

/// 后端 → 前端的「关窗确认」事件名（前端在 App.vue 里监听）。
pub const CLOSE_DIALOG_EVENT: &str = "wb:close-dialog";

const MENU_SHOW: &str = "wb_tray_show";
const MENU_QUIT: &str = "wb_tray_quit";

/// 平台差异（单测可显式传入，运行时用 [`PLATFORM`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Windows,
}

impl Platform {
    fn is_mac(self) -> bool {
        matches!(self, Platform::MacOS)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::MacOS => "macos",
            Platform::Windows => "windows",
        }
    }
}

/// 运行时平台（非 macOS 一律按 Windows 语义处理）。
pub const PLATFORM: Platform = if cfg!(target_os = "macos") {
    Platform::MacOS
} else {
    Platform::Windows
};

/// 用户在设置里存的「未启用托盘时的关窗行为」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// 收进系统托盘（顺手开启托盘）。
    Tray,
    /// 隐藏窗口、保留 Dock（仅 macOS 有意义）。
    Hide,
    /// 直接退出应用。
    Quit,
}

impl CloseAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseAction::Tray => "tray",
            CloseAction::Hide => "hide",
            CloseAction::Quit => "quit",
        }
    }
    fn code(self) -> u8 {
        match self {
            CloseAction::Tray => 1,
            CloseAction::Hide => 2,
            CloseAction::Quit => 3,
        }
    }
    fn from_code(code: u8) -> Option<CloseAction> {
        match code {
            1 => Some(CloseAction::Tray),
            2 => Some(CloseAction::Hide),
            3 => Some(CloseAction::Quit),
            _ => None,
        }
    }
}

/// 严格解析（命令层写侧校验用）。
pub fn parse_close_action(s: &str) -> Option<CloseAction> {
    match s {
        "tray" => Some(CloseAction::Tray),
        "hide" => Some(CloseAction::Hide),
        "quit" => Some(CloseAction::Quit),
        _ => None,
    }
}

/// 原始存储值 + 平台 → 实际生效的关窗行为。
///
/// - 缺省 / 非法 → 平台默认（macOS `hide`、Windows `quit`）
/// - `hide` 在 Windows 上没有「隐藏保留任务栏」这一档 → 回落 `quit`
pub fn effective_close_action(raw: Option<CloseAction>, platform: Platform) -> CloseAction {
    match raw {
        Some(CloseAction::Tray) => CloseAction::Tray,
        Some(CloseAction::Quit) => CloseAction::Quit,
        Some(CloseAction::Hide) | None => {
            if platform.is_mac() {
                CloseAction::Hide
            } else {
                CloseAction::Quit
            }
        }
    }
}

/// 点关闭按钮时应该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// 弹「收进托盘 / 隐藏 / 退出」确认窗（异步等用户选择）。
    ShowTrayDialog,
    /// 开启托盘（若尚未开启）并隐藏窗口。
    HideToTray,
    /// 隐藏窗口但保留 Dock 图标（仅 macOS）。
    HideKeepDock,
    /// 直接退出应用。
    Quit,
    /// 已在退出流程中：放行系统默认关闭。
    AllowQuit,
}

/// 关窗决策（纯函数）。
///
/// 优先级：退出流程 > 托盘开启 > 询问 > 用户配置。
pub fn resolve_close_decision(
    platform: Platform,
    tray_on: bool,
    confirm: bool,
    action: CloseAction,
    quitting: bool,
) -> CloseDecision {
    if quitting {
        return CloseDecision::AllowQuit;
    }
    if tray_on {
        return CloseDecision::HideToTray;
    }
    if confirm {
        return CloseDecision::ShowTrayDialog;
    }
    match action {
        CloseAction::Tray => CloseDecision::HideToTray,
        CloseAction::Quit => CloseDecision::Quit,
        CloseAction::Hide => {
            if platform.is_mac() {
                CloseDecision::HideKeepDock
            } else {
                CloseDecision::Quit
            }
        }
    }
}

/// 确认窗的选择 → 动作（纯函数）。
///
/// `None`（Esc / 直接关掉 / 非法值）走**默认按钮语义**：macOS 隐藏保留 Dock、
/// Windows 退出程序。`"hide"` 同理在 Windows 回落退出。
pub fn resolve_dialog_decision(platform: Platform, raw: Option<&str>) -> CloseDecision {
    match raw.and_then(parse_close_action) {
        Some(CloseAction::Tray) => CloseDecision::HideToTray,
        Some(CloseAction::Quit) => CloseDecision::Quit,
        Some(CloseAction::Hide) | None => {
            if platform.is_mac() {
                CloseDecision::HideKeepDock
            } else {
                CloseDecision::Quit
            }
        }
    }
}

/// 托盘图标被点击时的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayClickAction {
    /// 左键抬起 → 显示 / 聚焦主窗口。
    ShowMainWindow,
    /// 右键 → 由系统弹出菜单（显示主窗口 / 退出）。
    OpenMenu,
    /// 其它（按下、移动）→ 忽略。
    Ignore,
}

pub fn tray_click_action(left_button: bool, button_up: bool) -> TrayClickAction {
    if !left_button {
        TrayClickAction::OpenMenu
    } else if button_up {
        TrayClickAction::ShowMainWindow
    } else {
        TrayClickAction::Ignore
    }
}

/// 切换开关时需要做的副作用（改设置后立即生效，无需重启）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraySyncPlan {
    pub create_tray: bool,
    pub remove_tray: bool,
    /// 恢复 Dock（macOS）图标；窗口可见时 Dock 本就应该显示。
    pub restore_dock: bool,
}

/// 由「切换前 → 切换后」推导副作用；同值不重复创建/移除（幂等）。
pub fn sync_plan(prev: bool, next: bool) -> TraySyncPlan {
    match (prev, next) {
        (false, true) => TraySyncPlan {
            create_tray: true,
            remove_tray: false,
            restore_dock: false,
        },
        (true, false) => TraySyncPlan {
            create_tray: false,
            remove_tray: true,
            restore_dock: true,
        },
        _ => TraySyncPlan {
            create_tray: false,
            remove_tray: false,
            restore_dock: false,
        },
    }
}

// ================= 配置段（kv["window"]） =================

/// 窗口/托盘配置（`kv["window"]`）。默认值只活在代码里，读接口不落库。
///
/// `close_action` 是**可选**键：缺省时按平台默认（macOS hide / Windows quit），
/// 故 sanitize 只在值合法时保留它（非法值等同于缺省，不写库）。
pub struct WindowSec;

impl ConfigSection for WindowSec {
    fn key(&self) -> &'static str {
        "window"
    }
    fn defaults(&self) -> Value {
        json!({ "tray_enabled": true, "close_confirm": true })
    }
    fn sanitize(&self, merged: Value) -> Result<Value, String> {
        let mut out = json!({
            "tray_enabled": merged.get("tray_enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            "close_confirm": merged.get("close_confirm").and_then(|v| v.as_bool()).unwrap_or(true),
        });
        if let Some(action) = merged
            .get("close_action")
            .and_then(|v| v.as_str())
            .and_then(parse_close_action)
        {
            out["close_action"] = json!(action.as_str());
        }
        Ok(out)
    }
}

/// 配置的原始语义（`close_action` 未解析平台默认，便于单测覆盖回落）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowCfg {
    pub tray_enabled: bool,
    pub close_confirm: bool,
    pub close_action_raw: Option<CloseAction>,
}

/// 从已合并的 JSON 文档解析出 [`WindowCfg`]。
pub fn window_cfg_from(v: &Value) -> WindowCfg {
    WindowCfg {
        tray_enabled: v
            .get("tray_enabled")
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
        close_confirm: v
            .get("close_confirm")
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
        close_action_raw: v
            .get("close_action")
            .and_then(|x| x.as_str())
            .and_then(parse_close_action),
    }
}

/// 命令层字段：把 [`WindowCfg`] 转成前端 API 形状（驼峰）。
/// `closeAction` 返回**平台实际生效值**（缺省时就是平台默认）。
pub fn window_to_api(cfg: &WindowCfg, platform: Platform) -> Value {
    json!({
        "tray_enabled": cfg.tray_enabled,
        "closeConfirm": cfg.close_confirm,
        "closeAction": effective_close_action(cfg.close_action_raw, platform).as_str(),
        "platform": platform.as_str(),
    })
}

/// 前端 API 形状（驼峰）→ kv 补丁（蛇形）。非法 `closeAction` 报错；缺省键不进补丁。
/// 供宿主在 `tray_set_config` 命令里调用。
pub fn config_patch_from_api(patch: &Value) -> Result<Map<String, Value>, String> {
    let mut kv_patch = Map::new();
    if let Some(v) = patch.get("tray_enabled") {
        kv_patch.insert("tray_enabled".to_string(), v.clone());
    }
    if let Some(v) = patch.get("closeConfirm") {
        kv_patch.insert("close_confirm".to_string(), v.clone());
    }
    if let Some(v) = patch.get("closeAction") {
        let s = v
            .as_str()
            .ok_or_else(|| "closeAction 必须是字符串（tray | hide | quit）".to_string())?;
        let action = parse_close_action(s)
            .ok_or_else(|| format!("closeAction 非法：{s}（只接受 tray / hide / quit）"))?;
        kv_patch.insert("close_action".to_string(), json!(action.as_str()));
    }
    Ok(kv_patch)
}

// ================= 运行时控制器 =================

/// 托盘与主窗口相关资源（宿主注入，避免 kit 依赖任何具体图标/文案）。
#[derive(Clone)]
pub struct WindowAssets {
    /// 主窗口标签（默认 `"main"`）。
    pub main_label: &'static str,
    /// 托盘唯一 id（也用于运行时移除）。
    pub tray_id: &'static str,
    /// 托盘 tooltip 文案。
    pub tray_tooltip: String,
    /// 托盘菜单「显示主窗口」文案。
    pub menu_show_label: String,
    /// 托盘菜单「退出」文案。
    pub menu_quit_label: String,
    /// macOS 模板图（单色 + alpha）；非 macOS 忽略，改用默认窗口图标。
    pub mac_tray_png: Option<Vec<u8>>,
}

impl Default for WindowAssets {
    fn default() -> Self {
        WindowAssets {
            main_label: "main",
            tray_id: "main-tray",
            tray_tooltip: String::new(),
            menu_show_label: "显示主窗口".to_string(),
            menu_quit_label: "退出".to_string(),
            mac_tray_png: None,
        }
    }
}

/// 窗口/托盘运行时控制器：封装全部内存态（托盘开关、关窗配置、退出流程、确认窗序列），
/// 并通过 [`tauri::Manager::manage`] 注册为托管状态。命令与宿主回调经由
/// `tauri::State<WindowController>` 访问。
///
/// 内部需要跨线程操作窗口的逻辑，统一通过 `app.try_state::<WindowController>()`
/// 在目标线程重新取回，避免把 `&self` 捕获进 `'static` 线程闭包。
pub struct WindowController {
    assets: WindowAssets,
    tray_enabled: AtomicBool,
    close_confirm: AtomicBool,
    close_action_raw: AtomicU8,
    quitting: AtomicBool,
    close_ui_ready: AtomicBool,
    close_seq: AtomicU64,
    close_pending: Mutex<Option<u64>>,
}

impl WindowController {
    pub fn new(assets: WindowAssets) -> Self {
        WindowController {
            assets,
            tray_enabled: AtomicBool::new(true),
            close_confirm: AtomicBool::new(true),
            close_action_raw: AtomicU8::new(0),
            quitting: AtomicBool::new(false),
            close_ui_ready: AtomicBool::new(false),
            close_seq: AtomicU64::new(0),
            close_pending: Mutex::new(None),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(WindowAssets::default())
    }

    fn pending_lock(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.close_pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前生效的关窗行为（原始值 + 平台默认）。
    pub fn current_close_action(&self) -> CloseAction {
        effective_close_action(
            CloseAction::from_code(self.close_action_raw.load(Ordering::SeqCst)),
            PLATFORM,
        )
    }

    /// 当前关窗决策（事件闭包只读内存，不碰数据库）。
    pub fn current_close_decision(&self) -> CloseDecision {
        resolve_close_decision(
            PLATFORM,
            self.tray_enabled.load(Ordering::SeqCst),
            self.close_confirm.load(Ordering::SeqCst),
            self.current_close_action(),
            self.quitting.load(Ordering::SeqCst),
        )
    }

    /// 应用整套窗口配置：先刷新内存副本（窗口事件回调只读内存），再按托盘开关创建/移除。
    pub fn apply_config(&self, app: &AppHandle, cfg: WindowCfg) {
        self.close_confirm
            .store(cfg.close_confirm, Ordering::SeqCst);
        self.close_action_raw.store(
            cfg.close_action_raw.map(CloseAction::code).unwrap_or(0),
            Ordering::SeqCst,
        );
        self.apply_enabled(app, cfg.tray_enabled);
    }

    /// 应用托盘开关（启动时、改设置时都走这里）。所有窗口/托盘操作投递到主线程。
    /// 幂等：同值重复调用不会重建或误删。
    pub fn apply_enabled(&self, app: &AppHandle, enabled: bool) {
        let prev = self.tray_enabled.swap(enabled, Ordering::SeqCst);
        let plan = sync_plan(prev, enabled);
        if !plan.create_tray && !plan.remove_tray && !plan.restore_dock {
            return;
        }
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(ctrl) = handle.try_state::<WindowController>() {
                if plan.remove_tray {
                    ctrl.remove_tray(&handle);
                }
                if plan.restore_dock {
                    set_dock_visible(&handle, true);
                }
                if plan.create_tray {
                    if let Err(e) = ctrl.build_tray(&handle) {
                        eprintln!("[TRAY] 创建托盘失败: {e}");
                    }
                }
            }
        });
    }

    /// 启动时按配置创建托盘（存储就绪后调用；不改 Dock 策略 —— 启动阶段窗口仍需可见）。
    pub fn init_from_config(&self, app: &AppHandle, cfg: WindowCfg) {
        self.close_confirm
            .store(cfg.close_confirm, Ordering::SeqCst);
        self.close_action_raw.store(
            cfg.close_action_raw.map(CloseAction::code).unwrap_or(0),
            Ordering::SeqCst,
        );
        self.tray_enabled.store(cfg.tray_enabled, Ordering::SeqCst);
        let handle = app.clone();
        let enabled = cfg.tray_enabled;
        let _ = app.run_on_main_thread(move || {
            if let Some(ctrl) = handle.try_state::<WindowController>() {
                if enabled {
                    if let Err(e) = ctrl.build_tray(&handle) {
                        eprintln!("[TRAY] 创建托盘失败: {e}");
                    }
                } else {
                    ctrl.remove_tray(&handle);
                }
            }
        });
    }

    /// 确保托盘已开启（`close_action = "tray"` 的快速设置：落库 + 立即创建）。
    /// 需宿主传入 `&dyn KvStore` 写配置。
    fn ensure_tray_enabled(&self, app: &AppHandle, store: &dyn crate::config::KvStore) -> Result<(), String> {
        if self.tray_enabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.persist(app, store, json!({ "tray_enabled": true }))?;
        Ok(())
    }

    /// 写窗口配置并立即生效；返回写后的 [`WindowCfg`]。
    pub fn persist(
        &self,
        app: &AppHandle,
        store: &dyn crate::config::KvStore,
        patch: Value,
    ) -> Result<WindowCfg, String> {
        let out = crate::config::ConfigStore::set(&WindowSec, store, &patch)?;
        let cfg = window_cfg_from(&out);
        self.apply_config(app, cfg);
        Ok(cfg)
    }

    fn execute_decision(
        &self,
        app: &AppHandle,
        store: &dyn crate::config::KvStore,
        decision: CloseDecision,
    ) {
        match decision {
            CloseDecision::AllowQuit => {}
            CloseDecision::HideToTray => {
                if let Err(e) = self.ensure_tray_enabled(app, store) {
                    eprintln!("[TRAY] 开启托盘失败（仍按收进托盘处理）: {e}");
                }
                self.hide_to_tray(app);
            }
            CloseDecision::HideKeepDock => self.hide_keep_dock(app),
            CloseDecision::Quit => self.quit(app),
            CloseDecision::ShowTrayDialog => self.show_tray_dialog(app),
        }
    }

    /// `CloseRequested` 的唯一入口：同步算出决策，非放行一律先 `prevent_close()`，
    /// 再执行（询问分支只发事件，不会阻塞主线程等用户）。
    pub fn on_close_requested(
        &self,
        app: &AppHandle,
        store: &dyn crate::config::KvStore,
        api: &CloseRequestApi,
    ) {
        let decision = self.current_close_decision();
        if decision == CloseDecision::AllowQuit {
            return;
        }
        api.prevent_close();
        self.execute_decision(app, store, decision);
    }

    /// 弹关窗确认窗：前端未就绪时直接按默认按钮语义执行；已有弹窗时不再叠加。
    fn show_tray_dialog(&self, app: &AppHandle) {
        let mut pending = self.pending_lock();
        if pending.is_some() {
            return;
        }
        if !self.close_ui_ready.load(Ordering::SeqCst) {
            drop(pending);
            self.execute_decision(
                app,
                // 弹窗分支不写配置，store 仅占位；用空实现避免签名膨胀。
                &NoopKv,
                resolve_dialog_decision(PLATFORM, None),
            );
            return;
        }
        let id = self.close_seq.fetch_add(1, Ordering::SeqCst) + 1;
        *pending = Some(id);
        drop(pending);

        let payload = json!({ "requestId": id, "platform": PLATFORM.as_str() });
        if let Err(e) = app.emit(CLOSE_DIALOG_EVENT, payload) {
            eprintln!("[TRAY] 发送关窗确认事件失败: {e}");
            *self.pending_lock() = None;
            self.execute_decision(app, &NoopKv, resolve_dialog_decision(PLATFORM, None));
        }
    }

    /// 前端确认窗挂好监听后调用：之后才允许「询问」分支走事件（否则按默认动作执行）。
    pub fn close_dialog_ready(&self) {
        self.close_ui_ready.store(true, Ordering::SeqCst);
    }

    /// 确认窗回填：`choice` = `tray` | `hide` | `quit`（缺失/非法 = 默认按钮语义）。
    ///
    /// `remember = true` 时写 `close_confirm=false`，并把本次选择写入 `close_action`
    /// （`tray` 也算一次记住的选择）；`requestId` 不匹配当前等待项时直接忽略（防重入）。
    pub fn close_dialog_resolve(
        &self,
        app: &AppHandle,
        store: &dyn crate::config::KvStore,
        request_id: u64,
        choice: Option<String>,
        remember: bool,
    ) -> Result<(), String> {
        let matched = {
            let mut pending = self.pending_lock();
            match pending.as_ref() {
                Some(id) if *id == request_id => {
                    *pending = None;
                    true
                }
                _ => false,
            }
        };
        if !matched {
            return Ok(());
        }

        let raw = choice.as_deref();
        if remember {
            let mut patch = Map::new();
            patch.insert("close_confirm".to_string(), json!(false));
            if let Some(action) = raw.and_then(parse_close_action) {
                // Windows 上 `hide` 无意义，统一存平台有效值，避免配置里留下死档
                let stored = if PLATFORM.is_mac() || action != CloseAction::Hide {
                    action
                } else {
                    CloseAction::Quit
                };
                patch.insert("close_action".to_string(), json!(stored.as_str()));
            }
            self.persist(app, store, Value::Object(patch))?;
        }

        self.execute_decision(app, store, resolve_dialog_decision(PLATFORM, raw));
        Ok(())
    }

    /// 真正退出应用：先置 QUITTING（放行后续关闭），再退出。
    pub fn quit(&self, app: &AppHandle) {
        self.quitting.store(true, Ordering::SeqCst);
        app.exit(0);
    }

    /// 标记「真正退出」流程：让后续关闭请求直接放行（不再被隐藏/询问拦截）。
    ///
    /// 区别于 [`WindowController::quit`]：本方法只置标志、不主动退出，供宿主在
    /// `RunEvent::ExitRequested` 里调用（真正退出由系统默认关闭流程完成）。
    pub fn mark_quitting(&self) {
        self.quitting.store(true, Ordering::SeqCst);
    }

    /// 收进托盘：隐藏窗口，Dock（macOS）/ 任务栏（Windows）一并消失，仅留托盘。
    pub fn hide_to_tray(&self, app: &AppHandle) {
        if let Some(win) = app.get_webview_window(self.assets.main_label) {
            let _ = win.hide();
        }
        set_taskbar_visible(app, false);
        set_dock_visible(app, false);
    }

    /// 隐藏窗口但保留 Dock 图标（macOS）：点 Dock 图标经 `RunEvent::Reopen` 恢复。
    pub fn hide_keep_dock(&self, app: &AppHandle) {
        set_dock_visible(app, true);
        if let Some(win) = app.get_webview_window(self.assets.main_label) {
            let _ = win.hide();
        }
    }

    /// 显示 / 聚焦于主窗口（托盘单击、菜单「显示主窗口」、macOS Reopen）。
    pub fn show_main(&self, app: &AppHandle) {
        set_dock_visible(app, true);
        set_taskbar_visible(app, true);
        if let Some(win) = app.get_webview_window(self.assets.main_label) {
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    }

    /// macOS Reopen（Dock 点击）→ 显示主窗口。
    pub fn on_reopen(&self, app: &AppHandle) {
        self.show_main(app);
    }
}

/// 弹窗分支不写配置时用的占位存储（emit 失败兜底按默认动作执行，不涉及写库）。
struct NoopKv;
impl crate::config::KvStore for NoopKv {
    fn kv_get(&self, _key: &str) -> Option<Value> {
        None
    }
    fn kv_set(&self, _key: &str, _value: &Value) {}
    fn kv_delete(&self, _key: &str) {}
}

// ================= 托盘构建 / 平台副作用（自由函数） =================

/// macOS：Regular（有 Dock 图标/菜单栏）↔ Accessory（仅托盘，无 Dock 图标）。
#[cfg(target_os = "macos")]
fn set_dock_visible(app: &AppHandle, show: bool) {
    let policy = if show {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    if let Err(e) = app.set_activation_policy(policy) {
        eprintln!("[TRAY] set_activation_policy(show={show}) 失败: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn set_dock_visible(_app: &AppHandle, _show: bool) {}

/// Windows：收进托盘时从任务栏消失，显示时恢复。
#[cfg(target_os = "windows")]
fn set_taskbar_visible(app: &AppHandle, visible: bool) {
    if let Some(win) = app.get_webview_window("main") {
        if let Err(e) = win.set_skip_taskbar(!visible) {
            eprintln!("[TRAY] set_skip_taskbar({}) 失败: {e}", !visible);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn set_taskbar_visible(_app: &AppHandle, _visible: bool) {}

impl WindowController {
    /// 创建托盘（幂等：已存在则直接返回）。必须在主线程调用。
    fn build_tray(&self, app: &AppHandle) -> tauri::Result<()> {
        if app.tray_by_id(self.assets.tray_id).is_some() {
            return Ok(());
        }

        let show = MenuItem::with_id(app, MENU_SHOW, &self.assets.menu_show_label, true, None::<&str>)?;
        let quit_item =
            MenuItem::with_id(app, MENU_QUIT, &self.assets.menu_quit_label, true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&show, &quit_item])?;

        let mut builder = TrayIconBuilder::with_id(self.assets.tray_id)
            .tooltip(&self.assets.tray_tooltip)
            .menu(&menu)
            // 左键不弹菜单（改为直接显示窗口），右键仍由系统弹菜单
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| {
                if let Some(ctrl) = app.try_state::<WindowController>() {
                    match event.id.as_ref() {
                        MENU_SHOW => ctrl.show_main(app),
                        MENU_QUIT => ctrl.quit(app),
                        _ => {}
                    }
                }
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } = event
                {
                    let action = tray_click_action(
                        button == MouseButton::Left,
                        button_state == MouseButtonState::Up,
                    );
                    if action == TrayClickAction::ShowMainWindow {
                        if let Some(ctrl) = tray.app_handle().try_state::<WindowController>() {
                            ctrl.show_main(tray.app_handle());
                        }
                    }
                }
            });

        #[cfg(target_os = "macos")]
        {
            match &self.assets.mac_tray_png {
                Some(bytes) => {
                    builder = builder
                        .icon(tauri::image::Image::from_bytes(bytes)?)
                        .icon_as_template(true);
                }
                None => {
                    eprintln!("[TRAY] 未注入 macOS 模板图标，托盘使用系统占位图标");
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Windows 用彩色应用图标；模板模式是 macOS 专属语义，这里不设
            match app.default_window_icon().cloned() {
                Some(icon) => builder = builder.icon(icon),
                None => eprintln!("[TRAY] 未取到默认窗口图标，托盘使用系统占位图标"),
            }
        }

        builder.build(app)?;
        Ok(())
    }

    /// 移除托盘（幂等）。
    fn remove_tray(&self, app: &AppHandle) {
        if app.remove_tray_by_id(self.assets.tray_id).is_some() {
            eprintln!("[TRAY] 已按设置移除托盘");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KvStore;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemKv(Mutex<HashMap<String, Value>>);
    impl MemKv {
        fn new() -> Self {
            MemKv(Mutex::new(HashMap::new()))
        }
    }
    impl KvStore for MemKv {
        fn kv_get(&self, key: &str) -> Option<Value> {
            self.0.lock().unwrap().get(key).cloned()
        }
        fn kv_set(&self, key: &str, value: &Value) {
            self.0.lock().unwrap().insert(key.to_string(), value.clone());
        }
        fn kv_delete(&self, key: &str) {
            self.0.lock().unwrap().remove(key);
        }
    }

    // ---------- 行为矩阵：托盘开启（两平台一致） ----------
    #[test]
    fn tray_on_hides_to_tray_and_never_asks() {
        for platform in [Platform::MacOS, Platform::Windows] {
            for confirm in [true, false] {
                for action in [CloseAction::Tray, CloseAction::Hide, CloseAction::Quit] {
                    assert_eq!(
                        resolve_close_decision(platform, true, confirm, action, false),
                        CloseDecision::HideToTray,
                        "托盘开启时不弹窗、不参与 close_action"
                    );
                }
            }
        }
    }

    #[test]
    fn mac_no_tray_with_confirm_asks() {
        for action in [CloseAction::Tray, CloseAction::Hide, CloseAction::Quit] {
            assert_eq!(
                resolve_close_decision(Platform::MacOS, false, true, action, false),
                CloseDecision::ShowTrayDialog,
                "close_confirm=true 时 close_action 不参与决策"
            );
        }
    }

    #[test]
    fn mac_no_tray_default_or_hide_keeps_dock() {
        assert_eq!(
            resolve_close_decision(Platform::MacOS, false, false, CloseAction::Hide, false),
            CloseDecision::HideKeepDock
        );
        assert_eq!(
            effective_close_action(None, Platform::MacOS),
            CloseAction::Hide
        );
    }

    #[test]
    fn mac_no_tray_quit_exits() {
        assert_eq!(
            resolve_close_decision(Platform::MacOS, false, false, CloseAction::Quit, false),
            CloseDecision::Quit
        );
    }

    #[test]
    fn mac_no_tray_tray_enables_and_hides() {
        assert_eq!(
            resolve_close_decision(Platform::MacOS, false, false, CloseAction::Tray, false),
            CloseDecision::HideToTray
        );
    }

    #[test]
    fn win_no_tray_with_confirm_asks() {
        for action in [CloseAction::Tray, CloseAction::Hide, CloseAction::Quit] {
            assert_eq!(
                resolve_close_decision(Platform::Windows, false, true, action, false),
                CloseDecision::ShowTrayDialog
            );
        }
    }

    #[test]
    fn win_no_tray_default_quits() {
        assert_eq!(
            effective_close_action(None, Platform::Windows),
            CloseAction::Quit
        );
    }

    #[test]
    fn win_no_tray_hide_falls_back_to_quit() {
        assert_eq!(
            effective_close_action(Some(CloseAction::Hide), Platform::Windows),
            CloseAction::Quit
        );
        assert_eq!(
            resolve_close_decision(Platform::Windows, false, false, CloseAction::Hide, false),
            CloseDecision::Quit
        );
    }

    #[test]
    fn win_no_tray_tray_enables_and_hides() {
        assert_eq!(
            resolve_close_decision(Platform::Windows, false, false, CloseAction::Tray, false),
            CloseDecision::HideToTray
        );
    }

    #[test]
    fn quitting_always_allows_close() {
        for platform in [Platform::MacOS, Platform::Windows] {
            for tray_on in [true, false] {
                for confirm in [true, false] {
                    for action in [CloseAction::Tray, CloseAction::Hide, CloseAction::Quit] {
                        assert_eq!(
                            resolve_close_decision(platform, tray_on, confirm, action, true),
                            CloseDecision::AllowQuit
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_close_action_falls_back() {
        assert_eq!(parse_close_action("bogus"), None);
        assert_eq!(parse_close_action(""), None);
        assert_eq!(parse_close_action("TRAY"), None);
        let s = MemKv::new();
        let out = crate::config::ConfigStore::set(&WindowSec, &s, &json!({"close_action": "bogus"})).unwrap();
        assert!(out.get("close_action").is_none(), "非法 close_action 不落库");
    }

    #[test]
    fn dialog_cancel_esc_and_invalid_use_default_button() {
        assert_eq!(
            resolve_dialog_decision(Platform::MacOS, None),
            CloseDecision::HideKeepDock
        );
        assert_eq!(
            resolve_dialog_decision(Platform::Windows, None),
            CloseDecision::Quit
        );
        assert_eq!(
            resolve_dialog_decision(Platform::MacOS, Some("bogus")),
            CloseDecision::HideKeepDock
        );
    }

    #[test]
    fn dialog_explicit_choices_map_to_actions() {
        assert_eq!(
            resolve_dialog_decision(Platform::MacOS, Some("tray")),
            CloseDecision::HideToTray
        );
        assert_eq!(
            resolve_dialog_decision(Platform::Windows, Some("hide")),
            CloseDecision::Quit,
            "Windows 不支持隐藏保留任务栏"
        );
    }

    #[test]
    fn tray_left_click_shows_main_window() {
        assert_eq!(tray_click_action(true, true), TrayClickAction::ShowMainWindow);
        assert_eq!(tray_click_action(true, false), TrayClickAction::Ignore);
    }

    #[test]
    fn tray_right_click_opens_menu() {
        assert_eq!(tray_click_action(false, true), TrayClickAction::OpenMenu);
        assert_eq!(tray_click_action(false, false), TrayClickAction::OpenMenu);
    }

    #[test]
    fn turning_tray_off_removes_icon_and_restores_dock() {
        let p = sync_plan(true, false);
        assert!(p.remove_tray);
        assert!(p.restore_dock);
        assert!(!p.create_tray);
    }

    #[test]
    fn same_setting_is_a_noop() {
        for (prev, next) in [(true, true), (false, false)] {
            let p = sync_plan(prev, next);
            assert!(!p.create_tray && !p.remove_tray && !p.restore_dock);
        }
    }

    #[test]
    fn config_defaults_and_roundtrip() {
        let s = MemKv::new();
        let cfg = crate::config::ConfigStore::get(&WindowSec, &s);
        assert_eq!(cfg.get("tray_enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            cfg.get("close_confirm").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(cfg.get("close_action").is_none(), "close_action 缺省 = 平台默认");
        assert!(s.kv_get("window").is_none(), "读不落库");

        let out =
            crate::config::ConfigStore::set(&WindowSec, &s, &json!({"tray_enabled": false})).unwrap();
        assert_eq!(
            out.get("tray_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn partial_patch_does_not_reset_saved_fields() {
        let s = MemKv::new();
        let out = crate::config::ConfigStore::set(
            &WindowSec,
            &s,
            &json!({"close_action": "quit", "close_confirm": false}),
        )
        .unwrap();
        assert_eq!(
            out.get("close_action").and_then(|v| v.as_str()),
            Some("quit")
        );
        let out =
            crate::config::ConfigStore::set(&WindowSec, &s, &json!({"tray_enabled": false})).unwrap();
        assert_eq!(
            out.get("close_action").and_then(|v| v.as_str()),
            Some("quit"),
            "旧前端只传 tray_enabled：close_action 不能被重置"
        );
    }
}
