//! 通用配置抽象层：所有持久化配置（kv 单文档）统一走这一层。
//!
//! ## 设计约定
//!
//! - **默认值只活在代码里**：读接口不落库；「恢复默认」= 删掉 kv 行（而不是把默认值写进去）。
//! - 每类配置实现 [`ConfigSection`]：声明 kv 键、代码内默认值与「校验 + 钳制」逻辑（sanitize）。
//!   读写统一走 [`ConfigStore`]。
//! - kv 键由宿主自行约定（如 `app` / `notify` / `window`），本层不预置任何业务键。
//!
//! ## 与宿主的解耦
//!
//! 本层不直接依赖宿主的数据库，而是通过 [`KvStore`] trait 读写「按 key 的 JSON 文档」。
//! 宿主只需让自己的存储实现 [`KvStore`]（通常委托已有的 `kv_get/kv_set/kv_delete`），
//! 即可零成本接入整套配置能力，无需引入第二个数据库。
//!
//! ## 合并语义
//!
//! `set` 先取「当前生效值」（默认值 ∪ 已保存），再与 patch 做**深合并**
//! （嵌套对象逐键合并），最后交给 sanitize 钳制后落库。get 只读不写。

use serde_json::Value;

/// 一类持久化配置的底层存储抽象。宿主实现它（通常委托现有 `kv_*` 方法），
/// 即可让 [`ConfigStore`] / 通知 / 窗口等模块读写配置，且不被业务库耦合。
pub trait KvStore: Send + Sync {
    /// 读一个 kv 文档；不存在或非法 JSON 返回 `None`。
    fn kv_get(&self, key: &str) -> Option<Value>;
    /// 写一个 kv 文档（整体覆盖）。
    fn kv_set(&self, key: &str, value: &Value);
    /// 删除一个 kv 文档。
    fn kv_delete(&self, key: &str);
}

/// 一类持久化配置段的抽象。
pub trait ConfigSection: Send + Sync {
    /// kv 表中的键（宿主自行约定，保持与历史一致即可）。
    fn key(&self) -> &'static str;
    /// 代码内默认值。库中无行 = 从未调整过，读出来的就是它。
    fn defaults(&self) -> Value;
    /// 校验 + 钳制：输入「默认值 ∪ 已保存 ∪ patch」的合并结果，
    /// 返回合法化后的完整配置。非法值回默认而非报错。
    fn sanitize(&self, merged: Value) -> Result<Value, String>;
}

/// 配置读写统一入口。无状态，全部经 `&dyn KvStore` 落库。
pub struct ConfigStore;

impl ConfigStore {
    /// 读当前生效配置：默认值 ∪ 已保存（读时合并，不落库）。
    pub fn get(sec: &dyn ConfigSection, store: &dyn KvStore) -> Value {
        let mut merged = sec.defaults();
        if let Some(saved) = store.kv_get(sec.key()) {
            deep_merge(&mut merged, &saved);
        }
        sec.sanitize(merged).unwrap_or_else(|_| sec.defaults())
    }

    /// 合并写入：深合并 patch → 钳制 → 落库，返回写入后的完整配置。
    pub fn set(
        sec: &dyn ConfigSection,
        store: &dyn KvStore,
        patch: &Value,
    ) -> Result<Value, String> {
        let mut merged = Self::get(sec, store);
        deep_merge(&mut merged, patch);
        let out = sec.sanitize(merged)?;
        store.kv_set(sec.key(), &out);
        Ok(out)
    }

    /// 恢复默认：**删掉 kv 行**（而非写入默认值），返回代码内默认值。
    pub fn reset(sec: &dyn ConfigSection, store: &dyn KvStore) -> Value {
        store.kv_delete(sec.key());
        sec.defaults()
    }
}

/// 深合并：同为对象时逐键递归，否则 patch 覆盖。
pub fn deep_merge(dst: &mut Value, patch: &Value) {
    match (dst, patch) {
        (Value::Object(d), Value::Object(p)) => {
            for (k, v) in p {
                match d.get_mut(k) {
                    Some(dv) if dv.is_object() && v.is_object() => deep_merge(dv, v),
                    _ => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (d, p) => *d = p.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 内存 KV 实现，便于本层自测，不引入任何业务存储。
    struct MemKv {
        map: Mutex<HashMap<String, Value>>,
    }

    impl MemKv {
        fn new() -> Self {
            MemKv {
                map: Mutex::new(HashMap::new()),
            }
        }
    }

    impl KvStore for MemKv {
        fn kv_get(&self, key: &str) -> Option<Value> {
            self.map.lock().unwrap().get(key).cloned()
        }
        fn kv_set(&self, key: &str, value: &Value) {
            self.map.lock().unwrap().insert(key.to_string(), value.clone());
        }
        fn kv_delete(&self, key: &str) {
            self.map.lock().unwrap().remove(key);
        }
    }

    struct SampleSec;
    impl ConfigSection for SampleSec {
        fn key(&self) -> &'static str {
            "sample"
        }
        fn defaults(&self) -> Value {
            json!({"enabled": true, "mem_mb": 64, "nested": {"a": 1}})
        }
        fn sanitize(&self, merged: Value) -> Result<Value, String> {
            let mb = merged
                .get("mem_mb")
                .and_then(|v| v.as_i64())
                .unwrap_or(64)
                .clamp(4, 1024);
            Ok(json!({
                "enabled": merged.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                "mem_mb": mb,
                "nested": merged.get("nested").cloned().unwrap_or(json!({"a": 1})),
            }))
        }
    }

    #[test]
    fn get_returns_defaults_without_row() {
        let s = MemKv::new();
        let cfg = ConfigStore::get(&SampleSec, &s);
        assert_eq!(cfg.get("mem_mb").and_then(|v| v.as_i64()), Some(64));
        // 读不落库：库中仍无行（与「默认值只活在代码里」一致）
        assert!(s.kv_get("sample").is_none());
    }

    #[test]
    fn set_merges_and_clamps() {
        let s = MemKv::new();
        let out = ConfigStore::set(&SampleSec, &s, &json!({"mem_mb": 9999})).unwrap();
        assert_eq!(out.get("mem_mb").and_then(|v| v.as_i64()), Some(1024));
        assert_eq!(
            out.get("enabled").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn reset_deletes_row_back_to_defaults() {
        let s = MemKv::new();
        ConfigStore::set(&SampleSec, &s, &json!({"enabled": false})).unwrap();
        assert!(s.kv_get("sample").is_some());
        let cfg = ConfigStore::reset(&SampleSec, &s);
        assert_eq!(cfg.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert!(s.kv_get("sample").is_none(), "恢复默认 = 删行");
    }

    #[test]
    fn deep_merge_replaces_scalar_with_object() {
        let mut dst = json!({"a": 1, "b": {"x": 1}});
        deep_merge(&mut dst, &json!({"b": {"y": 2}, "c": 3}));
        assert_eq!(dst, json!({"a": 1, "b": {"x": 1, "y": 2}, "c": 3}));
    }
}
