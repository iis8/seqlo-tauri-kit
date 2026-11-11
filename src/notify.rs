//! 系统通知与桌面悬浮提示的通用引擎，从桌面端 `notify.rs` 抽取。
//!
//! ## 通用边界
//!
//! 本模块只承载**与业务无关的路由与内存状态机**，业务相关的具体内容由宿主注入：
//!
//! - **配置段（[`config::ConfigSection`]）**：宿主提供 `NotifySec`（默认值 `defaults` 与
//!   校验钳制 `sanitize` 都知道自己的分类 / 事件目录，本模块不预置任何业务键）。
//! - **路由目录（[`NotifyCatalog`]）**：宿主提供「分类键 / 分类默认开关 / 事件 id 清单 /
//!   事件→分类映射 / 事件节流窗口 / 全局节流窗口」六类知识，纯函数 [`decide`] 据此路由。
//! - **告警中心（[`AlertSink`]）**：`dispatch` 在路由前会调用宿主注入的告警落库回调，
//!   把 [`Alert`] 写入业务告警中心（`crate::stats::alert` 等），本模块不耦合任何库表。
//! - **业务事件构造函数**（如开播 / 录制完成 / 批次完成）：留在宿主，调用
//!   [`NotifyController::dispatch`] 投递；它们依赖 `provider` / `stats` / `media_jobs` 等。
//!
//! ## 核心原则
//!
//! - **事件源统一收口**：业务代码只构造 [`Event`] 并交给 `dispatch`，不允许直接调系统 API。
//! - **前台 / 后台二选一**：前台只发应用内浮层，后台只发系统通知，同一事件不双弹。
//!   「前台」= 主窗口 `is_visible() && !is_minimized() && is_focused()`。
//! - **配置沿用 `kv["notify"]` + 宿主 `NotifySec`**：默认值只活在代码里，不新建表。
//! - **浮层不新造 emit 总线**：复用快照 `snapshot_json`，前端轮询取走（`poll` / `consume`）。
//! - 内存状态（待弹队列 / 摘要 / 最近 / 节流表）不落库，进程重启即清空，
//!   符合「待弹队列 3 条上限」的成本约束。

use chrono::Timelike;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::config::{ConfigSection, ConfigStore, KvStore};

// ================= 宿主注入：路由目录 =================

/// 路由所需的全部「业务知识」，由宿主在构造 [`NotifyController`] 时一次性提供。
///
/// 这些函数指针描述的是**宿主自己的事件 / 分类目录**，kit 不预置任何业务语义，
/// 因此可被任意 Tauri 2 项目直接复用。所有字段均为 `Copy`，故本结构可 `Copy`/`Clone`。
#[derive(Clone, Copy)]
pub struct NotifyCatalog {
    /// 全部已知分类键（如 `["record","download","collect","health"]`），用于
    /// [`Cfg::from_value`] 给缺省分类补默认开关。
    pub categories: &'static [&'static str],
    /// 某分类的默认 (sys, float) 开关（分类级默认值）。
    pub default_category_switch: fn(&str) -> (bool, bool),
    /// 全部已知事件 id（前端 `notify_event` 校验白名单）。
    pub event_ids: &'static [&'static str],
    /// 事件 id → 分类键（用于事件级覆盖缺省时回填分类默认）。
    pub event_category: fn(&str) -> &'static str,
    /// 事件 id → 节流窗口（秒，同 event+scope 在窗口内只发一次）。
    pub event_throttle: fn(&str) -> i64,
    /// 事件 id → 全局节流窗口（秒，跨 scope 去重），无则 `None`。
    pub global_throttle: fn(&str) -> Option<i64>,
}

// ================= 事件模型 =================

/// 业务事件的不可变描述。业务代码只负责构造它，路由逻辑全部在这里。
#[derive(Clone, Debug)]
pub struct Event {
    /// 事件 id（与设置页 events 开关、节流表对应）。
    pub id: String,
    /// 分类键（决定默认的 sys / float 开关）。
    pub category: &'static str,
    /// 是否重要（重要事件穿透勿扰、不受 only_important 抑制）。
    pub important: bool,
    /// 标题。
    pub title: String,
    /// 正文。
    pub body: String,
    /// 点击跳转目标（前端路由）。
    pub target: Value,
    /// 去重范围（同 event + 同 scope 在节流窗口内只发一次）。
    pub scope: String,
    /// 是否同时写告警中心（留痕）。
    pub alert: Option<Alert>,
}

impl Event {
    pub fn new(
        id: impl Into<String>,
        category: &'static str,
        important: bool,
        title: impl Into<String>,
        body: impl Into<String>,
        target: Value,
        scope: impl Into<String>,
    ) -> Event {
        Event {
            id: id.into(),
            category,
            important,
            title: title.into(),
            body: body.into(),
            target,
            scope: scope.into(),
            alert: None,
        }
    }

    /// 追加告警中心条目（落库由宿主 [`AlertSink`] 完成，本模块不碰库）。
    pub fn with_alert(mut self, alert: Alert) -> Event {
        self.alert = Some(alert);
        self
    }
}

/// 告警中心条目。字段语义与宿主告警中心对齐；`dedupe_sec` 控制去重窗口。
#[derive(Clone, Debug)]
pub struct Alert {
    pub kind: &'static str,
    pub title: String,
    pub detail: String,
    pub level: &'static str,
    pub dedupe_sec: Option<f64>,
}

// ================= 运行态配置 =================

/// `dispatch` 判定用的运行态配置（由 [`Cfg::from_value`] 从生效 JSON 解析）。
#[derive(Clone, Debug)]
pub struct Cfg {
    pub enabled: bool,
    pub only_important: bool,
    pub quiet_enabled: bool,
    pub quiet_start: String,
    pub quiet_end: String,
    pub quiet_allow_important: bool,
    pub sound: bool,
    pub click_to_open: bool,
    /// 分类 → (sys, float)。
    pub categories: HashMap<String, (bool, bool)>,
    /// 事件 → (sys, float)（已含分类默认回退）。
    pub events: HashMap<String, (bool, bool)>,
}

impl Cfg {
    /// 从生效配置 JSON 解析出 [`Cfg`]。分类默认开关取自 `catalog.default_category_switch`；
    /// 事件未显式覆盖时按 `event_category` 回填分类默认。
    pub fn from_value(v: &Value, catalog: &NotifyCatalog) -> Cfg {
        let getb = |v: &Value, k: &str, d: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(d);
        let qh = v.get("quiet_hours").cloned().unwrap_or(json!({}));
        let mut categories = HashMap::new();
        for c in catalog.categories {
            let d = (catalog.default_category_switch)(c);
            let src = v
                .get("categories")
                .and_then(|x| x.get(*c))
                .cloned()
                .unwrap_or(json!({}));
            categories.insert(
                c.to_string(),
                (getb(&src, "sys", d.0), getb(&src, "float", d.1)),
            );
        }
        let mut events = HashMap::new();
        if let Some(o) = v.get("events").and_then(|x| x.as_object()) {
            for (k, e) in o {
                let (sys, float) = (catalog.default_category_switch)((catalog.event_category)(k));
                events.insert(k.clone(), (getb(e, "sys", sys), getb(e, "float", float)));
            }
        }
        Cfg {
            enabled: getb(v, "enabled", true),
            only_important: getb(v, "only_important", false),
            quiet_enabled: getb(&qh, "enabled", false),
            quiet_start: qh
                .get("start")
                .and_then(|x| x.as_str())
                .unwrap_or("22:00")
                .into(),
            quiet_end: qh
                .get("end")
                .and_then(|x| x.as_str())
                .unwrap_or("08:00")
                .into(),
            quiet_allow_important: getb(&qh, "allow_important", true),
            sound: getb(v, "sound", true),
            click_to_open: getb(v, "click_to_open", true),
            categories,
            events,
        }
    }

    /// 事件最终开关：事件覆盖优先，否则分类默认。
    pub fn switches(&self, id: &str, cat: &str) -> (bool, bool) {
        self.events
            .get(id)
            .copied()
            .unwrap_or_else(|| self.categories.get(cat).copied().unwrap_or((true, true)))
    }
}

// ================= 纯工具（时间 / 勿扰窗口） =================

fn now_sec() -> i64 {
    chrono::Local::now().timestamp()
}

fn minute_of_day() -> u32 {
    let n = chrono::Local::now();
    n.hour() * 60 + n.minute()
}

/// 校验一个 `HH:MM` 字符串；非法时回落 `fallback`。宿主在 `NotifySec::sanitize`
/// 里校验 `quiet_hours.start/end` 时直接复用，避免各项目重复实现。
pub fn normalize_hhmm(s: &str, fallback: &str) -> String {
    match parse_hhmm(s) {
        Some(_) => s.to_string(),
        None => fallback.to_string(),
    }
}

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// 判断某一分钟是否落在勿扰窗口内（支持跨零点）。
fn in_quiet_window(start: &str, end: &str, minute_of_day: u32) -> bool {
    let (Some((sh, sm)), Some((eh, em))) = (parse_hhmm(start), parse_hhmm(end)) else {
        return false;
    };
    let s = sh * 60 + sm;
    let e = eh * 60 + em;
    if s == e {
        return false;
    }
    if s < e {
        minute_of_day >= s && minute_of_day < e
    } else {
        minute_of_day >= s || minute_of_day < e
    }
}

// ================= 内存状态 =================

#[derive(Default)]
struct Digest {
    count: i64,
    by_category: HashMap<String, i64>,
    titles: Vec<String>,
}

#[derive(Default)]
struct Engine {
    pending: Vec<Value>,
    digest: Digest,
    recent: Vec<Value>,
    /// 节流时间戳（Unix 秒）。
    throttle: HashMap<String, i64>,
    /// 系统通知 id → (跳转目标, 生成时间)。
    sys_targets: HashMap<i32, (Value, i64)>,
    next_sys_id: i32,
    /// 系统通知被点击后暂存的目标，供前端轮询取走。
    open_target: Option<Value>,
    /// 直播开播时间（scope → Unix 秒），用于短场合并。
    live_started: HashMap<String, i64>,
}

/// 宿主注入的告警落库回调。默认 `None` 时不写告警中心（纯路由）。
///
/// 宿主通常传入一个闭包，内部通过自身的全局 `Store` 句柄调用告警中心写接口
/// （如 `crate::stats::alert`），从而把 [`Alert`] 落库留痕。
pub type AlertSink = Arc<dyn Fn(&dyn KvStore, &Alert, &str) + Send + Sync>;

/// 主机资源（目前只需主窗口标签；点击系统通知后聚焦它）。
#[derive(Clone)]
pub struct NotifyAssets {
    /// 主窗口标签（默认 `"main"`）。
    pub main_label: &'static str,
}

impl Default for NotifyAssets {
    fn default() -> Self {
        NotifyAssets {
            main_label: "main",
        }
    }
}

// ================= 运行时控制器 =================

/// 通知引擎托管状态。通过 [`tauri::Manager::manage`] 注册；命令与宿主业务构造函数
/// 经由 `tauri::State<NotifyController>` 访问。
///
/// 内部需要跨线程操作窗口 / 通知的逻辑，统一通过 `app.try_state::<NotifyController>()`
/// 在目标线程重新取回，避免把 `&self` 捕获进 `'static` 线程闭包。
pub struct NotifyController {
    /// 应用句柄（注册系统通知点击回调、判断前台、聚焦主窗口）。
    app: OnceLock<AppHandle>,
    catalog: NotifyCatalog,
    assets: NotifyAssets,
    /// 宿主提供的配置段（key / defaults / sanitize 都懂自己的分类与事件）。
    sec: &'static dyn ConfigSection,
    engine: Mutex<Engine>,
    alert_sink: Option<AlertSink>,
}

impl NotifyController {
    pub fn new(
        catalog: NotifyCatalog,
        assets: NotifyAssets,
        sec: &'static dyn ConfigSection,
        alert_sink: Option<AlertSink>,
    ) -> Self {
        NotifyController {
            app: OnceLock::new(),
            catalog,
            assets,
            sec,
            engine: Mutex::new(Engine::default()),
            alert_sink,
        }
    }

    /// 资源用默认、无告警落库（最简接入）。
    pub fn with_defaults(catalog: NotifyCatalog, sec: &'static dyn ConfigSection) -> Self {
        Self::new(catalog, NotifyAssets::default(), sec, None)
    }

    /// 应用启动时登记 AppHandle（供系统通知与前台判定使用）并注册点击回调。
    pub fn init(&self, app: &AppHandle) {
        let _ = self.app.set(app.clone());
        let handle = app.clone();
        let _ = app.notification().on_action(move |action| {
            if action.action_id() != "tap" {
                return;
            }
            let Some(n) = action.notification() else {
                return;
            };
            if let Some(ctrl) = handle.try_state::<NotifyController>() {
                ctrl.on_notification_action(n.id());
            }
        });
    }

    /// 主窗口是否真正在前台（不能只看 `is_visible`）。
    pub fn is_foreground(&self) -> bool {
        let Some(app) = self.app.get() else {
            return false;
        };
        let Some(w) = app.get_webview_window(self.assets.main_label) else {
            return false;
        };
        let visible = w.is_visible().unwrap_or(false);
        let minimized = w.is_minimized().unwrap_or(false);
        let focused = w.is_focused().unwrap_or(false);
        visible && !minimized && focused
    }

    /// 真正的投递：先写告警（若有），再判定，最后按出口落账。
    pub fn dispatch(&self, store: &dyn KvStore, ev: Event) {
        let cfg = Cfg::from_value(&ConfigStore::get(self.sec, store), &self.catalog);
        let now = now_sec();
        let minute = minute_of_day();
        let fg = self.is_foreground();

        if let Some(a) = &ev.alert {
            if let Some(sink) = &self.alert_sink {
                sink(store, a, &ev.scope);
            }
        }

        let outcome = {
            let engine = self.engine.lock().unwrap();
            decide(&cfg, &ev, now, minute, fg, &engine.throttle, &self.catalog)
        };

        let mut engine = self.engine.lock().unwrap();
        match outcome {
            Outcome::Silent => {}
            Outcome::Digested => {
                merge_digest(&mut engine, &ev);
                mark_throttle(&mut engine, &ev, now, &self.catalog);
            }
            Outcome::Float => {
                push_pending(&mut engine, &ev);
                mark_throttle(&mut engine, &ev, now, &self.catalog);
            }
            Outcome::System => {
                // 后台时系统通道失败（无权限 / 平台静默吞）→ 退回摘要，不丢事件。
                if self
                    .send_system(&mut engine, &ev, cfg.sound, cfg.click_to_open, now)
                    .is_ok()
                {
                    mark_throttle(&mut engine, &ev, now, &self.catalog);
                } else {
                    merge_digest(&mut engine, &ev);
                }
            }
        }
    }

    /// 开播登记（供短场合并判断）：记录 scope 的开播时间。
    pub fn mark_live_started(&self, scope: &str) {
        let mut e = self.engine.lock().unwrap();
        e.live_started.insert(scope.to_string(), now_sec());
    }

    /// 取某 scope 的开播时间（短场合并判断用）；未记录返回 `None`。
    pub fn live_started_at(&self, scope: &str) -> Option<i64> {
        self.engine
            .lock()
            .unwrap()
            .live_started
            .get(scope)
            .copied()
    }

    /// 供 `get_stats` 注入与 `notify_poll` 直接使用。
    pub fn snapshot_json(&self) -> Value {
        let e = self.engine.lock().unwrap();
        json!({
            "pending": e.pending,
            "digest": if e.digest.count > 0 {
                json!({
                    "count": e.digest.count,
                    "by_category": e.digest.by_category,
                    "titles": e.digest.titles,
                })
            } else { Value::Null },
            "recent": e.recent,
            "open_target": e.open_target,
        })
    }

    /// 消费：移除待弹 / 摘要 / 最近 / 点击目标。
    pub fn consume_ids(&self, ids: &[String]) {
        let mut e = self.engine.lock().unwrap();
        for id in ids {
            match id.as_str() {
                "__digest__" => e.digest = Digest::default(),
                "__recent__" => e.recent.clear(),
                "__open__" => e.open_target = None,
                _ => e
                    .pending
                    .retain(|p| p.get("id").and_then(|v| v.as_str()) != Some(id.as_str())),
            }
        }
    }

    /// 前端触发的事件（目前仅前端态任务使用；后端业务事件仍由宿主统一触发）。
    pub fn handle_frontend_event(&self, store: &dyn KvStore, event: Value) -> Result<(), String> {
        let id = event.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !self.catalog.event_ids.contains(&id) {
            return Err(format!("未知通知事件：{id}"));
        }
        let title = event.get("title").and_then(|v| v.as_str()).unwrap_or("通知");
        let body = event.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let important = event
            .get("important")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let scope = event
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("frontend");
        let target = event
            .get("target")
            .cloned()
            .unwrap_or(json!({"route": "/alerts"}));
        let cat = (self.catalog.event_category)(id);
        self.dispatch(
            store,
            Event::new(id, cat, important, title, body, target, scope),
        );
        Ok(())
    }

    /// 设置页「发一条测试通知」：不写告警中心、不参与节流、不进摘要。
    /// 无 AppHandle（未 init）时返回 `Err("no_app")`，便于宿主在测试期静默处理。
    pub fn test_notification(&self) -> Result<Value, String> {
        let cfg = Cfg::from_value(&self.sec.defaults(), &self.catalog);
        let cat = self.catalog.categories.first().copied().unwrap_or("health");
        let ev = Event::new(
            "kit_test",
            cat,
            false,
            "测试通知",
            "看到这条说明系统通知通道可用",
            json!({"route": "/settings"}),
            "test",
        );
        let mut engine = self.engine.lock().unwrap();
        self.send_system(&mut engine, &ev, cfg.sound, cfg.click_to_open, now_sec())?;
        Ok(json!({"ok": true}))
    }

    // ---------------- 内部：系统通知 / 点击 / 聚焦 ----------------

    fn on_notification_action(&self, id: i32) {
        let target = {
            let mut e = self.engine.lock().unwrap();
            e.sys_targets.remove(&id).map(|(t, _)| t)
        };
        if let Some(t) = target {
            let mut e = self.engine.lock().unwrap();
            e.open_target = Some(t);
        }
        if let Some(app) = self.app.get() {
            self.focus_main(app);
        }
    }

    fn focus_main(&self, app: &AppHandle) {
        if let Some(w) = app.get_webview_window(self.assets.main_label) {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
        }
    }

    fn send_system(
        &self,
        engine: &mut Engine,
        ev: &Event,
        sound: bool,
        click_to_open: bool,
        now: i64,
    ) -> Result<(), String> {
        let app = self.app.get().ok_or_else(|| "no_app".to_string())?;
        let id = engine.next_sys_id.max(1);
        engine.next_sys_id = id.saturating_add(1);
        if click_to_open {
            engine.sys_targets.insert(id, (ev.target.clone(), now));
            // 只保留最近 16 条映射，避免无限增长。
            if engine.sys_targets.len() > 16 {
                let oldest = engine
                    .sys_targets
                    .iter()
                    .min_by_key(|(_, (_, ts))| *ts)
                    .map(|(k, _)| *k);
                if let Some(k) = oldest {
                    engine.sys_targets.remove(&k);
                }
            }
        }
        push_recent(engine, ev, click_to_open, now);

        let mut b = app
            .notification()
            .builder()
            .id(id)
            .title(ev.title.clone())
            .body(ev.body.clone());
        if sound {
            b = b.sound("default");
        }
        b.show().map_err(|e| e.to_string())
    }
}

// ================= 路由（纯函数） =================

/// 前台 / 后台二选一的结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// 前台：推入应用内浮层队列。
    Float,
    /// 后台：发系统通知。
    System,
    /// 合并进「回来补摘要」。
    Digested,
    /// 不打扰（前台但浮层关 / 总开关关 / 被节流）。
    Silent,
}

/// 纯判定：不产生任何副作用，便于单测覆盖所有分支。
fn decide(
    cfg: &Cfg,
    ev: &Event,
    now: i64,
    minute: u32,
    foreground: bool,
    throttle: &HashMap<String, i64>,
    catalog: &NotifyCatalog,
) -> Outcome {
    if !cfg.enabled {
        return Outcome::Silent;
    }
    if cfg.only_important && !ev.important {
        return Outcome::Digested;
    }
    // 勿扰：普通事件丢弃即时通道，进次日摘要；重要事件按 allow_important 穿透。
    if cfg.quiet_enabled
        && in_quiet_window(&cfg.quiet_start, &cfg.quiet_end, minute)
        && !(ev.important && cfg.quiet_allow_important)
    {
        return Outcome::Digested;
    }
    // 节流：同 event + scope 在窗口内只发一次。
    let key = format!("{}:{}", ev.id, ev.scope);
    if let Some(ts) = throttle.get(&key) {
        if now.saturating_sub(*ts) < (catalog.event_throttle)(&ev.id) {
            return Outcome::Silent;
        }
    }
    if let Some(win) = (catalog.global_throttle)(&ev.id) {
        let gkey = format!("{}:__global__", ev.id);
        if let Some(ts) = throttle.get(&gkey) {
            if now.saturating_sub(*ts) < win {
                return Outcome::Silent;
            }
        }
    }

    let (sys, float) = cfg.switches(&ev.id, &ev.category);
    if foreground {
        if float {
            Outcome::Float
        } else {
            Outcome::Silent
        }
    } else if sys {
        Outcome::System
    } else {
        Outcome::Digested
    }
}

fn mark_throttle(engine: &mut Engine, ev: &Event, now: i64, catalog: &NotifyCatalog) {
    engine
        .throttle
        .insert(format!("{}:{}", ev.id, ev.scope), now);
    if (catalog.global_throttle)(&ev.id).is_some() {
        engine
            .throttle
            .insert(format!("{}:__global__", ev.id), now);
    }
}

fn merge_digest(engine: &mut Engine, ev: &Event) {
    let d = &mut engine.digest;
    d.count += 1;
    *d.by_category
        .entry(ev.category.to_string())
        .or_insert(0) += 1;
    if d.titles.len() < 3 && !d.titles.contains(&ev.title) {
        d.titles.push(ev.title.clone());
    }
}

fn push_pending(engine: &mut Engine, ev: &Event) {
    // 最多 3 条；超出把最旧的一条并入摘要计数，避免越堆越多。
    if engine.pending.len() >= 3 {
        if let Some(_old) = engine.pending.first().cloned() {
            engine.pending.remove(0);
            engine.digest.count += 1;
            *engine
                .digest
                .by_category
                .entry(ev.category.to_string())
                .or_insert(0) += 1;
        }
    }
    engine.pending.push(json!({
        "id": format!("n{}", engine.next_sys_id.max(0)),
        "event": ev.id,
        "category": ev.category,
        "important": ev.important,
        "title": ev.title,
        "body": ev.body,
        "target": ev.target,
    }));
    engine.next_sys_id = engine.next_sys_id.saturating_add(1);
}

fn push_recent(engine: &mut Engine, ev: &Event, click_to_open: bool, now: i64) {
    engine.recent.push(json!({
        "id": format!("r{now}"),
        "event": ev.id,
        "title": ev.title,
        "body": ev.body,
        "target": if click_to_open { ev.target.clone() } else { Value::Null },
        "ts": now,
    }));
    if engine.recent.len() > 5 {
        engine.recent.remove(0);
    }
}

// ================= 配置 / 权限 辅助（命令层包装用） =================

/// 读生效配置；存储未就绪时回落到代码内默认值（不报错、不阻塞）。
pub fn read_config_value(sec: &dyn ConfigSection, store: Option<&dyn KvStore>) -> Value {
    match store {
        Some(s) => ConfigStore::get(sec, s),
        None => sec.defaults(),
    }
}

/// 合并写入配置（深合并 + sanitize + 落库）。
pub fn write_config(
    sec: &dyn ConfigSection,
    store: &dyn KvStore,
    patch: &Value,
) -> Result<Value, String> {
    ConfigStore::set(sec, store, patch)
}

/// 恢复默认（删行）。存储未就绪时回代码默认值。
pub fn reset_config_value(sec: &dyn ConfigSection, store: Option<&dyn KvStore>) -> Value {
    match store {
        Some(s) => ConfigStore::reset(sec, s),
        None => sec.defaults(),
    }
}

/// 桌面端插件文档明确：`permission_state` 在桌面恒为 Granted；
/// 真正的系统级开关只能由用户在系统设置里查看，这里不假装能探测。
pub fn permission_status() -> Value {
    json!({"granted": true, "desktop": true})
}

/// 向系统请求通知权限（仅桌面有效）。
pub fn request_permission(app: &AppHandle) -> Value {
    let granted = match app.notification().request_permission() {
        Ok(tauri_plugin_notification::PermissionState::Granted) => true,
        _ => false,
    };
    json!({"granted": granted})
}

/// 打开系统通知设置页（仅桌面）。
pub fn open_notification_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.Notifications-Settings.extension")
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统设置失败：{e}"))
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "ms-settings:notifications"]);
        // Windows 控制台子系统进程默认会弹黑窗；CREATE_NO_WINDOW 静默执行。
        cmd.creation_flags(0x08000000);
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统设置失败：{e}"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("当前平台暂不支持自动打开系统通知设置".into())
    }
}

// ================= 单测 =================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KvStore;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 内存 KV 实现，便于本模块自测，不引入任何业务存储。
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

    // ---- 样例目录（镜像宿主语义，足够覆盖路由与配置解析） ----
    fn sample_switch(cat: &str) -> (bool, bool) {
        match cat {
            "collect" => (true, false),
            _ => (true, true),
        }
    }
    fn sample_category(id: &str) -> &'static str {
        match id {
            "live_started" | "record_done" | "record_failed" => "record",
            "download_done" => "download",
            "collect_done" => "collect",
            _ => "health",
        }
    }
    fn sample_throttle(id: &str) -> i64 {
        match id {
            "live_started" => 1800,
            "download_done" => 60,
            _ => 600,
        }
    }
    fn sample_global(id: &str) -> Option<i64> {
        match id {
            "download_done" => Some(60),
            _ => None,
        }
    }
    static SAMPLE_CATALOG: NotifyCatalog = NotifyCatalog {
        categories: &["record", "download", "collect", "health"],
        default_category_switch: sample_switch,
        event_ids: &[
            "live_started",
            "record_done",
            "record_failed",
            "download_done",
            "collect_done",
            "health_event",
        ],
        event_category: sample_category,
        event_throttle: sample_throttle,
        global_throttle: sample_global,
    };

    struct SampleSec;
    impl ConfigSection for SampleSec {
        fn key(&self) -> &'static str {
            "notify"
        }
        fn defaults(&self) -> Value {
            json!({
                "enabled": true,
                "only_important": false,
                "quiet_hours": {"enabled": false, "start": "22:00", "end": "08:00", "allow_important": true},
                "sound": true,
                "click_to_open": true,
                "categories": {
                    "record":   {"sys": true,  "float": true},
                    "download": {"sys": true,  "float": true},
                    "collect":  {"sys": true,  "float": false},
                    "health":   {"sys": true,  "float": true}
                },
                // 事件专项覆盖只保存「与分类默认不同」的项。
                "events": {
                    "download_done": {"sys": false, "float": false}
                }
            })
        }
        fn sanitize(&self, merged: Value) -> Result<Value, String> {
            let getb =
                |v: &Value, k: &str, d: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(d);
            let enabled = getb(&merged, "enabled", true);
            let only_important = getb(&merged, "only_important", false);
            let sound = getb(&merged, "sound", true);
            let click_to_open = getb(&merged, "click_to_open", true);
            let qh = merged.get("quiet_hours").cloned().unwrap_or(json!({}));
            let q_enabled = getb(&qh, "enabled", false);
            let q_start = normalize_hhmm(
                qh.get("start").and_then(|v| v.as_str()).unwrap_or("22:00"),
                "22:00",
            );
            let q_end =
                normalize_hhmm(qh.get("end").and_then(|v| v.as_str()).unwrap_or("08:00"), "08:00");
            let q_allow_important = getb(&qh, "allow_important", true);
            let cats_in = merged.get("categories").cloned().unwrap_or(json!({}));
            let mut categories = serde_json::Map::new();
            for c in SAMPLE_CATALOG.categories {
                let d = sample_switch(c);
                let src = cats_in.get(*c).cloned().unwrap_or(json!({}));
                categories.insert(
                    c.to_string(),
                    json!({"sys": getb(&src, "sys", d.0), "float": getb(&src, "float", d.1)}),
                );
            }
            let ev_in = merged.get("events").cloned().unwrap_or(json!({}));
            let mut events = serde_json::Map::new();
            for id in SAMPLE_CATALOG.event_ids {
                if let Some(src) = ev_in.get(*id) {
                    let (sys, float) = sample_switch(sample_category(id));
                    events.insert(
                        id.to_string(),
                        json!({"sys": getb(src, "sys", sys), "float": getb(src, "float", float)}),
                    );
                }
            }
            Ok(json!({
                "enabled": enabled,
                "only_important": only_important,
                "quiet_hours": {
                    "enabled": q_enabled,
                    "start": q_start,
                    "end": q_end,
                    "allow_important": q_allow_important
                },
                "sound": sound,
                "click_to_open": click_to_open,
                "categories": Value::Object(categories),
                "events": Value::Object(events)
            }))
        }
    }
    static SAMPLE_SEC: SampleSec = SampleSec;

    fn reset_engine(c: &NotifyController) {
        *c.engine.lock().unwrap() = Engine::default();
    }

    fn ev(id: &'static str, important: bool) -> Event {
        Event::new(
            id,
            sample_category(id),
            important,
            "标题",
            "正文",
            json!({"route": "/tasks"}),
            "scope-a",
        )
    }

    fn controller() -> NotifyController {
        NotifyController::with_defaults(SAMPLE_CATALOG, &SAMPLE_SEC)
    }

    #[test]
    fn quiet_window_supports_overnight_and_boundaries() {
        assert!(in_quiet_window("22:00", "08:00", 23 * 60));
        assert!(in_quiet_window("22:00", "08:00", 2 * 60));
        assert!(!in_quiet_window("22:00", "08:00", 12 * 60));
        assert!(in_quiet_window("22:00", "08:00", 22 * 60)); // 含起点
        assert!(!in_quiet_window("22:00", "08:00", 8 * 60)); // 不含终点
        assert!(in_quiet_window("09:00", "18:00", 12 * 60));
        assert!(!in_quiet_window("09:00", "18:00", 8 * 60));
        assert!(!in_quiet_window("10:00", "10:00", 10 * 60)); // 起止相同 = 关闭
    }

    #[test]
    fn decide_routes_foreground_float_and_background_system() {
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &MemKv::new()), &SAMPLE_CATALOG);
        let empty = HashMap::new();
        assert_eq!(
            decide(
                &cfg,
                &ev("record_done", false),
                100,
                600,
                true,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Float
        );
        assert_eq!(
            decide(
                &cfg,
                &ev("record_done", false),
                100,
                600,
                false,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::System
        );
    }

    #[test]
    fn collect_float_is_off_in_foreground_but_system_on_in_background() {
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &MemKv::new()), &SAMPLE_CATALOG);
        let empty = HashMap::new();
        assert_eq!(
            decide(
                &cfg,
                &ev("collect_done", false),
                100,
                600,
                true,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Silent
        );
        assert_eq!(
            decide(
                &cfg,
                &ev("collect_done", false),
                100,
                600,
                false,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::System
        );
    }

    #[test]
    fn single_download_defaults_to_silent_both_ways() {
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &MemKv::new()), &SAMPLE_CATALOG);
        let empty = HashMap::new();
        assert_eq!(
            decide(
                &cfg,
                &ev("download_done", false),
                100,
                600,
                true,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Silent
        );
        // 后台 + 系统关 → 进摘要（而不是丢）
        assert_eq!(
            decide(
                &cfg,
                &ev("download_done", false),
                100,
                600,
                false,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Digested
        );
    }

    #[test]
    fn important_event_passes_quiet_hours_but_normal_does_not() {
        let s = MemKv::new();
        let _ = write_config(
            &SAMPLE_SEC,
            &s,
            &json!({"quiet_hours": {"enabled": true, "start": "22:00", "end": "08:00", "allow_important": true}}),
        );
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &s), &SAMPLE_CATALOG);
        let minute = 23 * 60;
        let empty = HashMap::new();
        assert_eq!(
            decide(
                &cfg,
                &ev("record_failed", true),
                100,
                minute,
                false,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::System
        );
        assert_eq!(
            decide(
                &cfg,
                &ev("record_done", false),
                100,
                minute,
                false,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Digested
        );
    }

    #[test]
    fn only_important_digests_normal_events() {
        let s = MemKv::new();
        let _ = write_config(&SAMPLE_SEC, &s, &json!({"only_important": true}));
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &s), &SAMPLE_CATALOG);
        let empty = HashMap::new();
        assert_eq!(
            decide(
                &cfg,
                &ev("record_done", false),
                100,
                600,
                true,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Digested
        );
        assert_eq!(
            decide(
                &cfg,
                &ev("record_failed", true),
                100,
                600,
                true,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Float
        );
    }

    #[test]
    fn master_switch_off_is_silent() {
        let s = MemKv::new();
        let _ = write_config(&SAMPLE_SEC, &s, &json!({"enabled": false}));
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &s), &SAMPLE_CATALOG);
        let empty = HashMap::new();
        assert_eq!(
            decide(
                &cfg,
                &ev("record_failed", true),
                100,
                600,
                false,
                &empty,
                &SAMPLE_CATALOG
            ),
            Outcome::Silent
        );
    }

    #[test]
    fn throttle_suppresses_same_event_scope_within_window() {
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &MemKv::new()), &SAMPLE_CATALOG);
        let mut throttle = HashMap::new();
        throttle.insert("live_started:scope-a".to_string(), 100_i64);
        assert_eq!(
            decide(
                &cfg,
                &ev("live_started", true),
                100 + 1799,
                600,
                false,
                &throttle,
                &SAMPLE_CATALOG
            ),
            Outcome::Silent
        );
        assert_eq!(
            decide(
                &cfg,
                &ev("live_started", true),
                100 + 1800,
                600,
                false,
                &throttle,
                &SAMPLE_CATALOG
            ),
            Outcome::System
        );
    }

    #[test]
    fn global_throttle_suppresses_across_scopes() {
        let s = MemKv::new();
        let _ = write_config(
            &SAMPLE_SEC,
            &s,
            &json!({"events": {"download_done": {"sys": true, "float": true}}}),
        );
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &s), &SAMPLE_CATALOG);
        let mut throttle = HashMap::new();
        throttle.insert("download_done:__global__".to_string(), 100_i64);
        let mut e = ev("download_done", false);
        e.scope = "other-scope".into();
        assert_eq!(
            decide(&cfg, &e, 130, 600, false, &throttle, &SAMPLE_CATALOG),
            Outcome::Silent
        );
        assert_eq!(
            decide(&cfg, &e, 160, 600, false, &throttle, &SAMPLE_CATALOG),
            Outcome::System
        );
    }

    #[test]
    fn pending_cap_three_merges_overflow_into_digest() {
        let c = controller();
        reset_engine(&c);
        for i in 0..5 {
            let mut e = ev("record_done", false);
            e.scope = format!("s{i}");
            let mut engine = c.engine.lock().unwrap();
            push_pending(&mut engine, &e);
        }
        let snap = c.snapshot_json();
        assert_eq!(
            snap.get("pending").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(3)
        );
        assert_eq!(
            snap.pointer("/digest/count").and_then(|v| v.as_i64()),
            Some(2)
        );
        reset_engine(&c);
    }

    #[test]
    fn digest_merges_counts_and_keeps_three_titles() {
        let c = controller();
        reset_engine(&c);
        for i in 0..5 {
            let mut e = ev("record_done", false);
            e.title = format!("标题{i}");
            let mut engine = c.engine.lock().unwrap();
            merge_digest(&mut engine, &e);
        }
        let snap = c.snapshot_json();
        assert_eq!(
            snap.pointer("/digest/count").and_then(|v| v.as_i64()),
            Some(5)
        );
        assert_eq!(
            snap.pointer("/digest/by_category/record")
                .and_then(|v| v.as_i64()),
            Some(5)
        );
        assert_eq!(
            snap.pointer("/digest/titles")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(3)
        );
        reset_engine(&c);
    }

    #[test]
    fn consume_removes_pending_digest_recent_and_open_target() {
        let c = controller();
        reset_engine(&c);
        {
            let mut engine = c.engine.lock().unwrap();
            push_pending(&mut engine, &ev("record_failed", true));
            merge_digest(&mut engine, &ev("collect_done", false));
            engine.recent.push(json!({"id": "r1"}));
            engine.open_target = Some(json!({"route": "/tasks"}));
            engine.next_sys_id = 1;
        }
        let snap = c.snapshot_json();
        let pending_id = snap
            .pointer("/pending/0/id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        c.consume_ids(&[
            pending_id,
            "__digest__".into(),
            "__recent__".into(),
            "__open__".into(),
        ]);
        let snap = c.snapshot_json();
        assert_eq!(
            snap.get("pending").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
        assert!(snap.get("digest").unwrap().is_null());
        assert_eq!(
            snap.get("recent").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(0)
        );
        assert!(snap.get("open_target").unwrap().is_null());
        reset_engine(&c);
    }

    #[test]
    fn click_to_open_off_keeps_recent_without_target() {
        let c = controller();
        reset_engine(&c);
        {
            let mut engine = c.engine.lock().unwrap();
            push_recent(&mut engine, &ev("live_started", true), false, 100);
            let rec = engine.recent.first().cloned().unwrap();
            assert!(rec.get("target").unwrap().is_null());
        }
        reset_engine(&c);
    }

    #[test]
    fn config_reads_fall_back_to_defaults_without_store() {
        let v = read_config_value(&SAMPLE_SEC, None);
        assert_eq!(v.get("enabled").and_then(|x| x.as_bool()), Some(true));
        assert_eq!(
            v.pointer("/categories/collect/float").and_then(|x| x.as_bool()),
            Some(false)
        );
        let cfg = Cfg::from_value(&v, &SAMPLE_CATALOG);
        assert!(cfg.sound);
        assert!(cfg.click_to_open);
    }

    #[test]
    fn test_notification_without_app_reaches_send_path() {
        let c = controller();
        reset_engine(&c);
        let r = c.test_notification();
        assert_eq!(r.unwrap_err(), "no_app");
        reset_engine(&c);
    }

    #[test]
    fn frontend_event_rejects_unknown_id() {
        let c = controller();
        let r = c.handle_frontend_event(&MemKv::new(), json!({"id": "bogus_event"}));
        assert!(r.is_err());
        let r = c.handle_frontend_event(&MemKv::new(), json!({"id": "collect_done"}));
        assert!(r.is_ok());
    }

    #[test]
    fn unknown_frontend_event_is_rejected_shape() {
        assert!(!SAMPLE_CATALOG.event_ids.contains(&"bogus_event"));
        assert!(SAMPLE_CATALOG.event_ids.contains(&"collect_done"));
    }

    #[test]
    fn switches_falls_back_to_category_default() {
        let cfg = Cfg::from_value(&ConfigStore::get(&SAMPLE_SEC, &MemKv::new()), &SAMPLE_CATALOG);
        // collect_done 无事件级覆盖 → 跟随分类 collect 默认 (sys=true, float=false)
        assert_eq!(cfg.switches("collect_done", "collect"), (true, false));
        // download_done 有事件级覆盖 (sys=false, float=false)
        assert_eq!(cfg.switches("download_done", "download"), (false, false));
    }
}
