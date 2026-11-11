# vc-desktop-kit

Tauri 2 桌面端通用基础框架。提供与业务无关的五大能力，供 `video-crawl-desktop`
及任意其它 Tauri 2 项目复用：

| 模块 | 能力 |
|---|---|
| [`config`] | 通用 KV 配置抽象：`KvStore` trait、`ConfigSection` 配置段、`ConfigStore` 读/写/重置、`deep_merge` |
| [`migration`] | SQLite 版本化迁移器（简版 Flyway：BASELINE + checksum + 事务回滚） |
| [`splash`] | 启动页双窗口汇合状态机、macOS alpha 握手、原生主题同步 |
| [`window`] | 系统托盘、关窗行为决策矩阵、托盘/关窗配置读写 |
| [`notify`] | 系统通知/应用内浮层引擎：路由、勿扰、节流、待弹队列、点击跳转 |

## 设计边界（重要）

kit 只承载**通用框架**；业务内容（配置默认值、事件目录、迁移 SQL、托盘菜单文案、
图标资源）一律留在宿主，通过 trait / 函数指针 / 参数注入传入。kit 不读写任何
具体业务表，不内嵌任何业务文案。

宿主通过实现 **`KvStore` trait** 把自己的 KV 存储委托给 kit——kit 定义接口，
宿主实现它，kit 不新增数据库、零业务耦合。

## 接入

```toml
# 宿主 Cargo.toml
[dependencies]
vc-desktop-kit = { path = "libs/vc-desktop-kit" }
```

---

## config：KV 配置委托

### 1) 宿主实现 KvStore（把现有 KV 表交给 kit）

```rust
use serde_json::Value;
use vc_desktop_kit::config::KvStore;

impl KvStore for Store {
    fn kv_get(&self, key: &str) -> Option<Value> { self.kv_get(key) }   // 委托现有方法
    fn kv_set(&self, key: &str, value: &Value) { self.kv_set(key, value) }
    fn kv_delete(&self, key: &str) { self.kv_delete(key) }
}
```

> 注意：把 `&Arc<Store>` 传给需要 `&dyn KvStore` 的参数时，需显式解引用
> `&*st.store`（`Arc` 不会自动 coerce 成 trait 对象）。

### 2) 宿主定义配置段（默认值与校验都在宿主）

```rust
use serde_json::{json, Value};
use vc_desktop_kit::config::{ConfigSection, ConfigStore};

pub struct QueueSec;
impl ConfigSection for QueueSec {
    fn key(&self) -> &'static str { "queue" }            // kv 表中的键
    fn defaults(&self) -> Value { json!({"pages": 3}) }  // 代码内默认值
    fn sanitize(&self, merged: Value) -> Result<Value, String> {
        // 非法值回默认而非报错；返回合法化后的完整配置
        Ok(merged)
    }
}

// 读写（store: &dyn KvStore，实际传 &*st.store）
let cfg = ConfigStore::get(&QueueSec, store);
ConfigStore::set(&QueueSec, store, &json!({"pages": 5}))?;
ConfigStore::reset(&QueueSec, store);   // 删行回到 defaults
```

语义约定：**库中无行 = 从未调整过，读到的就是 `defaults()`**；`set` 走
`deep_merge`（嵌套合并、不丢兄弟字段）+ `sanitize` 钳制；`reset` 删行。

---

## migration：SQLite 版本化迁移

宿主只登记注册表（SQL 用 `include_str!` 编译期内嵌），引擎全部在 kit：

```rust
use vc_desktop_kit::migration::{run_with, Migration, DEFAULT_BASELINE};

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 4,
        name: "add_xxx",                                    // 文件名 V4__add_xxx.sql
        sql: include_str!("../migrations/V4__add_xxx.sql"),
        already_applied: None,                              // 带外缝合探针，通常 None
    },
];

// 启动时（建库后）：逐版本升序执行未应用脚本，失败整体回滚并返回 Err
run_with(&mut conn, MIGRATIONS, BASELINE_VERSION)?;
```

- `schema_history` 表记录 BASELINE 与每个已执行脚本（含 checksum）；
- checksum 不匹配 / 出现程序不包含的版本 → 拒绝启动（防「改老脚本」与「降级」）；
- 每脚本一个事务，DDL 亦可事务化，失败回滚后重启可安全重试；
- `validate_registry(MIGRATIONS, BASELINE)` 供测试静态校验注册表。

---

## splash：启动页双窗口状态机

### 前提（不可变形约束，详见 AGENTS.md「启动页不可变形约束」）

- `tauri.conf.json` 中 main / splash 都 `visible: false` + 静态浅色 `backgroundColor`
  （**不要**运行时 `set_background_color` 按主题取色，暗色下 splash 会黑屏）；
- splash 的 show 必须由 `Builder::on_page_load(PageLoadEvent::Finished)` 触发；
- 关闭 splash 必须 STORAGE_READY ∧ FRONTEND_READY 双汇合；
- `SPLASH_MIN_DURATION = 900ms` 不可删/缩短。

### 宿主接线（lib.rs）

```rust
use vc_desktop_kit::splash::SplashController;

.setup(|app| {
    app.manage(SplashController::with_defaults());
    // 三道兜底：5s show 兜底 / 20s watchdog（宿主起线程），
    // 1.5s 显形兜底在 kit 内部。
    Ok(())
})
.on_page_load(|webview, payload| {
    if webview.label() == "splash" && payload.event() == tauri::webview::PageLoadEvent::Finished {
        let app = webview.app_handle();
        app.try_state::<SplashController>()
            .map(|c| c.on_splash_page_loaded(app.clone()));   // alpha=0 → show
    }
})
```

```rust
// 三个 Tauri 命令（在 invoke_handler 注册，前端 invoke 调用）：
#[tauri::command]
fn splash_painted(app: AppHandle, ctrl: tauri::State<SplashController>) {
    ctrl.on_splash_painted(app);            // splash.html 双 rAF 上报：alpha 0→1 + focus
}
#[tauri::command]
fn frontend_ready(app: AppHandle, theme: Option<String>, ctrl: tauri::State<SplashController>) {
    ctrl.on_frontend_ready(app, theme.as_deref());   // 与 storage_ready 汇合后关 splash
}
#[tauri::command]
fn set_window_theme(app: AppHandle, theme: String, ctrl: tauri::State<SplashController>)
    -> Result<String, String> { ctrl.set_theme(app, &theme) }

// 后台建库线程完成时（不能挪回 setup 同步执行）：
ctrl.mark_storage_ready(&app);
// 兜底线程：5s 后 ctrl.show_fallback(&app)；20s 后 ctrl.force_finish(&app)。
```

---

## window：托盘与关窗决策

决策矩阵（纯函数，可单测）与执行（托盘/Dock/窗口操作）分离：

```rust
use vc_desktop_kit::window::{WindowAssets, WindowController, CloseRequestApi};

.setup(|app| {
    // 图标/文案等资源由宿主注入；默认文案与窗口标签用 with_defaults()
    app.manage(WindowController::new(WindowAssets {
        mac_tray_png: Some(include_bytes!("../icons/trayTemplate.png").to_vec()), // macOS 模板图
        tray_tooltip: "Video Crawl".into(),
        ..WindowAssets::default()   // main_label / tray_id / 菜单文案取默认
    }));
    Ok(())
})
```

```rust
// 存储就绪后按 kv["window"] 配置创建托盘：
let ctrl = app.try_state::<WindowController>().unwrap();
ctrl.init_from_config(&app.handle(), cfg);

// 主窗口 CloseRequested 里同步决策（不阻塞主线程）：
win.on_window_event(move |event| {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        ctrl.on_close_requested(&handle, api);   // 托盘开→隐藏收图标；确认→发事件；退出→放行
    }
});

// RunEvent::ExitRequested（托盘菜单退出 / Cmd+Q / 注销）先置标志：
ctrl.mark_quitting(app_handle);   // 之后触发的关窗不再被「隐藏」拦截
// macOS Dock 重新点开：ctrl.on_reopen(&app) → 显示并置前。
```

托盘/关窗配置的读写走同一套 `ConfigSection` 抽象（`WindowSec`，键 `window`），
`WindowCfg` / `window_to_api` / `config_patch_from_api` 负责 JSON ↔ 结构转换。

---

## notify：通知引擎

### 宿主契约

1. **`NotifyCatalog`**：六类路由知识（纯函数指针），宿主提供：

```rust
use vc_desktop_kit::notify::NotifyCatalog;

pub static CATALOG: NotifyCatalog = NotifyCatalog {
    categories: &["record", "download", "collect", "health"],
    default_category_switch: |cat| my_default_switch(cat),   // (sys, float)
    event_ids: &[EVENT_RECORD_DONE, EVENT_STORAGE_LOW, /* … */],
    event_category: |id| my_event_category(id),
    event_throttle: |id| my_event_throttle(id),              // 秒；同 event+scope 去重
    global_throttle: |id| my_global_throttle(id),            // 跨 scope 去重，可 None
};
```

2. **`NotifySec: ConfigSection`**（键 `notify`，含分类/事件开关默认值与 sanitize），
   以 `&'static` 引用传入（零大小类型 `static NOTIFY_SEC: NotifySec` 即可）。

3. **可选 `AlertSink`**：把 `Alert` 落库到业务告警中心：

```rust
use std::sync::Arc;
use vc_desktop_kit::notify::{AlertSink, NotifyController, NotifyAssets};

let sink: AlertSink = Arc::new(|store: &dyn KvStore, alert, scope| {
    stats::record_alert(store, alert, scope);   // 宿主业务落库
});
let ctrl = NotifyController::new(CATALOG, NotifyAssets::default(),  // CATALOG 是 Copy，直接传值
                                 &NOTIFY_SEC, Some(sink));
ctrl.init(app.handle());   // 先 init：注册点击回调（回调内走 try_state 取托管实例）
app.manage(ctrl);          // 再 manage：注册实例，供命令与后台线程取回
```

### 投递业务事件

业务代码构造 kit `Event`（或宿主领域 Event → 映射成 kit Event），一次调用完成
路由 / 勿扰 / 节流 / 前台浮层与后台系统通知分流 / 待弹队列：

```rust
use vc_desktop_kit::notify::{Event, Alert};

let ev = Event::new("record_done", "record", false,
                    "录制完成", "《xxx》 已保存", json!({"route": "/live"}),
                    "room:123")
    .with_alert(Alert { /* 重要事件同时留痕告警中心 */ .. });

if let Some(ctrl) = app.try_state::<NotifyController>() {
    ctrl.dispatch(&*store, ev);   // store: &dyn KvStore
}
```

### 命令层（宿主薄壳，逻辑都在 kit）

- 配置：`read_config_value` / `write_config` / `reset_config_value`；
- 待弹队列：`ctrl.snapshot_json()`（轮询）+ `ctrl.consume_ids(&ids)`（确认消费）；
- 前端回执：`ctrl.handle_frontend_event(&*store, event)`（白名单校验）；
- 测试与权限：`ctrl.test_notification()`、`permission_status()`、
  `request_permission(&app)`、`open_notification_settings()`。

直播节流需要跨重启记忆时：`ctrl.mark_live_started(&scope)` /
`ctrl.live_started_at(&scope)`（托管在控制器内，不依赖全局单例）。

---

## 测试

```bash
cargo test -p vc-desktop-kit    # 49 个单测：config/migration/splash/window/notify 引擎行为
```

引擎行为（路由矩阵、勿扰、节流、待弹合并、托盘决策矩阵、迁移回滚等）的测试
全部在 kit 内；宿主只保留「注入装配 + 业务默认值/事件构造」的少量测试。

## 已知宿主集成注意点

- `tauri::generate_context!()` 在编译期校验 `frontendDist` 路径存在——CI/新
  worktree 若无前端构建产物，需先 `pnpm build`（或放占位 `dist/index.html`）。
- 沙箱/受限环境可能禁止 spawn 外部命令（如 `ps`），依赖它的测试会失败，属
  环境限制而非代码问题。
