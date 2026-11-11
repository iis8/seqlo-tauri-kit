//! vc-desktop-kit：Tauri 2 桌面端通用基础框架。
//!
//! 提供与业务无关的通用能力，供 `video-crawl-desktop` 及其它 Tauri 2 项目复用：
//!
//! - [`config`]：通用 KV 配置抽象（`ConfigSection` / `ConfigStore` / `KvStore`）。
//! - [`migration`]：SQLite 版本化迁移器（简版 Flyway，零业务耦合）。
//! - [`splash`]：启动页（splash）状态机、macOS alpha 握手、原生主题同步。
//! - [`window`]：系统托盘与关窗行为决策矩阵。
//! - [`notify`]：系统通知 / 应用内浮层引擎（路由、勿扰、节流、待弹队列、点击跳转）。
//!
//! ## 设计边界
//!
//! 本 kit 只承载**通用框架**，业务相关的具体内容（配置默认值、事件目录、
//! 迁移 SQL、托盘菜单文案、图标资源）一律留在宿主，通过 trait / 参数 / 注入传入，
//! 保证 kit 可被任意 Tauri 2 项目直接引入。
//!
//! ### 通知模块（[`notify`]）的宿主契约
//!
//! - 宿主提供 [`notify::NotifyCatalog`]：分类键 / 分类默认开关 / 事件 id 白名单 /
//!   事件→分类映射 / 事件节流 / 全局节流（六类路由知识，纯函数指针）。
//! - 宿主提供 `NotifySec: ConfigSection`（含自己的 `defaults` 与 `sanitize`，
//!   知道分类与事件目录）；以 `&'static` 引用传入 [`notify::NotifyController::new`]。
//! - 宿主可选提供 [`notify::AlertSink`]：把 [`notify::Alert`] 落库到业务告警中心。
//! - 业务事件构造函数（开播 / 录制完成 / 批次完成等）留在宿主，调用
//!   [`notify::NotifyController::dispatch`] 投递。

pub mod config;
pub mod migration;
pub mod notify;
pub mod splash;
pub mod window;
