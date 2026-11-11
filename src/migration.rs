//! SQLite 版本化迁移（简版 Flyway，仅支持前向迁移，基于 sqlx + SQLite，异步）。
//!
//! 机制：
//! - 脚本由宿主以 [`Migration`] 列表提供（通常 `include_str!` 编译期内嵌进二进制，
//!   开发态与打包后行为一致，不依赖运行期文件路径）；
//! - `schema_history` 表记录一行 BASELINE 与每个已执行脚本（含 checksum）；
//! - 启动时按版本升序执行未应用脚本，每个脚本一个事务（SQLite 的 DDL 可事务化），
//!   失败整体回滚并返回错误（上层终止启动），重启可安全重试；
//! - 已应用脚本内容不可变：checksum 不匹配（或库里出现当前程序不包含的版本）即拒绝启动，
//!   防止「改老脚本」与「程序降级」两类事故。
//!
//! 注意：本模块全部入口都是 `async`，需在一个 tokio runtime 内调用
//! （Tauri 2 的 setup / 后台线程天然满足）。宿主侧统一使用 sqlx 的 [`SqlitePool`]，
//! 与业务库（global.db / project.db）同源、单一 SQLite 驱动。
//!
//! 宿主新增一次迁移：在 [`run`] / [`run_with`] 调用处登记一行
//! `Migration { version, name, sql, already_applied }` 即可；
//! `validate_registry` 会在启动时静态校验版本号递增与命名合法性。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// 默认存量基线版本：任何经过宿主 `init_schema()` 后都收敛到的形态。
/// 历史结构（v1..v3）无法准确考古回放，故首次见到本迁移器时直接写入
/// BASELINE@3，不执行任何脚本；其后的结构演进逐个版本补齐。宿主可按需覆盖。
pub const DEFAULT_BASELINE: i64 = 3;

const HISTORY_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_history (
  installed_rank INTEGER PRIMARY KEY AUTOINCREMENT,
  type           TEXT NOT NULL,                  -- BASELINE | SQL
  version        INTEGER NOT NULL UNIQUE,
  description    TEXT NOT NULL,
  script         TEXT NOT NULL,
  checksum       TEXT NOT NULL DEFAULT '',
  installed_on   TEXT NOT NULL DEFAULT '',
  execution_ms   INTEGER NOT NULL DEFAULT 0,
  success        INTEGER NOT NULL DEFAULT 1
);
"#;

/// 迁移探针：版本尚未登记时，宿主用它确认「结构变更已被更早的非版本化引导代码完成」，
/// 此时按当前 checksum 补登记为已应用、不执行脚本。因需查询数据库，故为异步。
pub type MigrationProbe = Arc<
    dyn for<'a> Fn(&'a SqlitePool) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> + Send + Sync,
>;

/// 一条可执行的版本化迁移。`sql` 通常由 `include_str!` 编译期内嵌（可含多条语句，按 `;` 拆分顺序执行）。
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
    /// 一次性「带外缝合」探针，通常为 `None`：
    /// 某些库在迁移机制正式建立前，已被引导代码改成该版本的后置形态（没有历史记录）。
    /// 版本尚未登记且探针返回 true 时，按当前 checksum 直接补登记为已应用、不执行脚本，
    /// 避免对这类库重跑 DDL 而失败（如重复加列）。
    pub already_applied: Option<MigrationProbe>,
}

impl Migration {
    /// 脚本在 `migrations/` 目录里的文件名（V<版本>__<描述>.sql）。
    pub fn script_name(&self) -> String {
        format!("V{}__{}.sql", self.version, self.name)
    }

    /// 脚本内容指纹（FNV-1a 64 位，非加密用途，仅做变更检测，零依赖）。
    fn checksum(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.sql.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{:016x}", hash)
    }
}

/// 启动入口（默认基线 = [`DEFAULT_BASELINE】）：建版本表、写基线、按序应用缺失脚本。
pub async fn run(pool: &SqlitePool, migrations: &[Migration]) -> Result<(), String> {
    run_with(pool, migrations, DEFAULT_BASELINE).await
}

/// 启动入口（自定义基线版本）：与 [`run`] 相同，但允许宿主指定存量基线版本号。
pub async fn run_with(
    pool: &SqlitePool,
    migrations: &[Migration],
    baseline: i64,
) -> Result<(), String> {
    sqlx::query(HISTORY_DDL)
        .execute(pool)
        .await
        .map_err(|e| format!("创建 schema_history 表失败: {e}"))?;
    ensure_baseline(pool, baseline).await?;
    validate_registry(migrations, baseline)?;
    reconcile_preapplied(pool, migrations).await?;

    // 已成功应用的 SQL 迁移：version -> checksum
    let applied: HashMap<i64, String> = {
        let rows = sqlx::query(
            "SELECT version, checksum FROM schema_history \
             WHERE type = 'SQL' AND success = 1",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
        let mut map = HashMap::new();
        for row in rows {
            let v: i64 = row.try_get(0).map_err(|e| e.to_string())?;
            let cs: String = row.try_get(1).map_err(|e| e.to_string())?;
            map.insert(v, cs);
        }
        map
    };

    // 防降级：库里的版本必须都能在当前程序的注册表中找到。
    for v in applied.keys() {
        if !migrations.iter().any(|m| m.version == *v) {
            return Err(format!(
                "数据库已应用当前程序不包含的迁移 V{}（疑似版本降级），拒绝启动",
                v
            ));
        }
    }

    for m in migrations {
        if let Some(old_checksum) = applied.get(&m.version) {
            if old_checksum != &m.checksum() {
                return Err(format!(
                    "迁移 {} 的内容与已执行记录不一致（checksum 冲突：已应用脚本不可修改）",
                    m.script_name()
                ));
            }
            continue;
        }
        apply_one(pool, m).await?;
    }
    Ok(())
}

/// 带外缝合：版本未登记但探针确认其结构变更已被更早的非版本化引导代码完成时，
/// 按当前 checksum 补一条成功记录，使后续正常流程跳过该脚本。仅做只读探测 + 插历史行。
async fn reconcile_preapplied(pool: &SqlitePool, migrations: &[Migration]) -> Result<(), String> {
    for m in migrations {
        let Some(probe) = &m.already_applied else {
            continue;
        };
        let done: i64 = sqlx::query(
            "SELECT COUNT(*) FROM schema_history WHERE type='SQL' AND version=? AND success=1",
        )
        .bind(m.version)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?
        .try_get(0)
        .map_err(|e| e.to_string())?;
        if done > 0 || !probe(pool).await {
            continue;
        }
        sqlx::query(
            "INSERT INTO schema_history \
               (type, version, description, script, checksum, installed_on, execution_ms, success) \
             VALUES ('SQL', ?, ?, ?, ?, ?, 0, 1)",
        )
        .bind(m.version)
        .bind(m.name)
        .bind(m.script_name())
        .bind(m.checksum())
        .bind(now_str())
        .execute(pool)
        .await
        .map_err(|e| format!("补登记迁移 V{} 失败: {}", m.version, e))?;
        eprintln!(
            "[migration] 检测到 V{} 的结构变更已就位，补登记为已应用（不执行脚本）",
            m.version
        );
    }
    Ok(())
}

/// 版本表为空时写入 BASELINE（存量库与全新库都由 init_schema 收敛到该形态）。
async fn ensure_baseline(pool: &SqlitePool, baseline: i64) -> Result<(), String> {
    let count: i64 = sqlx::query("SELECT COUNT(*) FROM schema_history")
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?
        .try_get(0)
        .map_err(|e| e.to_string())?;
    if count > 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO schema_history \
           (type, version, description, script, checksum, installed_on, execution_ms, success) \
         VALUES ('BASELINE', ?, 'baseline', 'BASELINE', '', ?, 0, 1)",
    )
    .bind(baseline)
    .bind(now_str())
    .execute(pool)
    .await
    .map_err(|e| format!("写入基线版本记录失败: {e}"))?;
    eprintln!("[migration] 版本表初始化，写入基线 V{}", baseline);
    Ok(())
}

/// 注册表静态约束：版本大于基线、严格递增、命名合法。
///
/// 宿主可在测试里调用本函数校验自己的 [`Migration`] 注册表（版本号递增与命名合法性），
/// 提前红灯「漏注册 / 命名不符」。
pub fn validate_registry(migrations: &[Migration], baseline: i64) -> Result<(), String> {
    let mut prev = baseline;
    for m in migrations {
        if m.version <= baseline {
            return Err(format!(
                "迁移版本号必须大于基线 V{}：{}",
                baseline,
                m.script_name()
            ));
        }
        if m.version <= prev {
            return Err(format!(
                "迁移版本号必须严格递增（V{} 出现在 V{} 之后）",
                m.version, prev
            ));
        }
        let valid_name = !m.name.is_empty()
            && !m.name.contains("__")
            && m
                .name
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
        if !valid_name {
            return Err(format!(
                "迁移名称只能含小写字母/数字/单个下划线：{}",
                m.script_name()
            ));
        }
        prev = m.version;
    }
    Ok(())
}

/// 执行单个迁移：整个脚本包在一个事务里（按 `;` 拆成多条语句顺序执行），任一步失败即回滚（含已执行的 DDL）。
async fn apply_one(pool: &SqlitePool, m: &Migration) -> Result<(), String> {
    let name = m.script_name();
    let started = Instant::now();
    eprintln!("[migration] 开始执行 {} ...", name);

    let outcome: Result<(), String> = async {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        for stmt in split_statements(m.sql) {
            sqlx::query(stmt)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        sqlx::query(
            "INSERT INTO schema_history \
               (type, version, description, script, checksum, installed_on, execution_ms, success) \
             VALUES ('SQL', ?, ?, ?, ?, ?, ?, 1)",
        )
        .bind(m.version)
        .bind(m.name)
        .bind(name.clone())
        .bind(m.checksum())
        .bind(now_str())
        .bind(started.elapsed().as_millis() as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;

    match outcome {
        Ok(()) => {
            eprintln!(
                "[migration] {} 完成（{}ms）",
                name,
                started.elapsed().as_millis()
            );
            Ok(())
        }
        Err(e) => Err(format!("迁移 {} 失败，已回滚: {}", name, e)),
    }
}

/// 把迁移脚本按顶层 `;` 拆成单条语句，忽略字符串字面量（`'...'` / `"..."`）内的分号。
/// 注释内的分号不特殊处理（迁移脚本通常为纯 DDL，风险可控）。
fn split_statements(sql: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        } else if c == ';' && !in_single && !in_double {
            let stmt = sql[start..i].trim();
            if !stmt.is_empty() {
                out.push(stmt);
            }
            start = i + 1;
        }
        i += 1;
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

    fn mg(version: i64, name: &'static str, sql: &'static str) -> Migration {
        Migration {
            version,
            name,
            sql,
            already_applied: None,
        }
    }

    fn mg_probed(
        version: i64,
        name: &'static str,
        sql: &'static str,
        probe: fn() -> bool,
    ) -> Migration {
        Migration {
            version,
            name,
            sql,
            already_applied: Some(Arc::new(
                move |_pool: &SqlitePool| -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
                    Box::pin(async move { (probe)() })
                },
            )),
        }
    }

    async fn open_mem() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory pool")
    }

    async fn history_versions(pool: &SqlitePool) -> Vec<(String, i64)> {
        let rows = sqlx::query("SELECT type, version FROM schema_history ORDER BY installed_rank")
            .fetch_all(pool)
            .await
            .unwrap();
        rows.into_iter()
            .map(|r| (r.get::<String, _>(0), r.get::<i64, _>(1)))
            .collect()
    }

    async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
            .get::<i64, _>(0);
        n > 0
    }

    #[tokio::test]
    async fn fresh_db_writes_baseline_and_is_idempotent() {
        let pool = open_mem().await;
        run_with(&pool, &[], DEFAULT_BASELINE).await.unwrap();
        assert_eq!(
            history_versions(&pool).await,
            vec![("BASELINE".to_string(), DEFAULT_BASELINE)]
        );
        // 再跑一次不重复写基线
        run_with(&pool, &[], DEFAULT_BASELINE).await.unwrap();
        assert_eq!(history_versions(&pool).await.len(), 1);
    }

    #[tokio::test]
    async fn applies_pending_migrations_in_order_and_skips_applied() {
        let pool = open_mem().await;
        let v4 = mg(
            4,
            "create_probe",
            "CREATE TABLE probe (id INTEGER); INSERT INTO probe VALUES (42);",
        );
        let v5 = mg(
            5,
            "add_probe_name",
            "ALTER TABLE probe ADD COLUMN name TEXT NOT NULL DEFAULT '';",
        );
        run_with(&pool, &[v4, v5], DEFAULT_BASELINE).await.unwrap();
        assert!(table_exists(&pool, "probe").await);
        let val: i64 = sqlx::query("SELECT id FROM probe")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get::<i64, _>(0);
        assert_eq!(val, 42);
        assert_eq!(
            history_versions(&pool).await,
            vec![
                ("BASELINE".to_string(), DEFAULT_BASELINE),
                ("SQL".to_string(), 4),
                ("SQL".to_string(), 5),
            ]
        );
        // 重启等价场景：同样的注册表再跑一遍，全部跳过、不产生重复记录
        let v4 = mg(
            4,
            "create_probe",
            "CREATE TABLE probe (id INTEGER); INSERT INTO probe VALUES (42);",
        );
        let v5 = mg(
            5,
            "add_probe_name",
            "ALTER TABLE probe ADD COLUMN name TEXT NOT NULL DEFAULT '';",
        );
        run_with(&pool, &[v4, v5], DEFAULT_BASELINE).await.unwrap();
        assert_eq!(history_versions(&pool).await.len(), 3);
    }

    #[tokio::test]
    async fn preapplied_probe_backfills_history_without_running_script() {
        let pool = open_mem().await;
        // 探针确认为后置形态：只补登记，不执行脚本（建表不应发生）
        let v4 = mg_probed(
            4,
            "already_done",
            "CREATE TABLE should_not_exist (x INTEGER);",
            || true,
        );
        run_with(&pool, &[v4], DEFAULT_BASELINE).await.unwrap();
        assert!(
            !table_exists(&pool, "should_not_exist").await,
            "探针命中时不得执行脚本"
        );
        assert_eq!(
            history_versions(&pool).await,
            vec![
                ("BASELINE".to_string(), DEFAULT_BASELINE),
                ("SQL".to_string(), 4),
            ]
        );
        // 再次运行：已有成功记录，checksum 匹配，正常跳过
        let v4 = mg_probed(
            4,
            "already_done",
            "CREATE TABLE should_not_exist (x INTEGER);",
            || true,
        );
        run_with(&pool, &[v4], DEFAULT_BASELINE).await.unwrap();
        assert_eq!(history_versions(&pool).await.len(), 2);

        // 探针返回 false 时走正常执行路径
        let pool2 = open_mem().await;
        let v4 = mg_probed(
            4,
            "needs_run",
            "CREATE TABLE really_created (x INTEGER);",
            || false,
        );
        run_with(&pool2, &[v4], DEFAULT_BASELINE).await.unwrap();
        assert!(
            table_exists(&pool2, "really_created").await,
            "探针未命中时必须执行脚本"
        );
    }

    #[tokio::test]
    async fn failing_migration_rolls_back_dml_and_ddl() {
        let pool = open_mem().await;
        let bad = mg(
            4,
            "bad_sql",
            // DDL + DML 后故意引用不存在的表：整个脚本（含建表）必须回滚
            "CREATE TABLE rollback_probe (id INTEGER); \
             INSERT INTO rollback_probe VALUES (1); \
             INSERT INTO no_such_table VALUES (1);",
        );
        let err = run_with(&pool, &[bad], DEFAULT_BASELINE)
            .await
            .unwrap_err();
        assert!(err.contains("V4__bad_sql.sql"), "错误信息: {err}");
        assert!(
            !table_exists(&pool, "rollback_probe").await,
            "DDL 应随事务回滚"
        );
        assert_eq!(
            history_versions(&pool).await,
            vec![("BASELINE".to_string(), DEFAULT_BASELINE)]
        );
    }

    #[tokio::test]
    async fn previous_migration_failure_does_not_block_later_success() {
        let pool = open_mem().await;
        let bad = mg(4, "bad_sql", "INSERT INTO no_such_table VALUES (1);");
        run_with(&pool, &[bad], DEFAULT_BASELINE)
            .await
            .unwrap_err();
        // 修正后重启：同一版本可重新应用
        let fixed = mg(4, "bad_sql", "CREATE TABLE fixed_probe (id INTEGER);");
        run_with(&pool, &[fixed], DEFAULT_BASELINE).await.unwrap();
        assert!(table_exists(&pool, "fixed_probe").await);
    }

    #[tokio::test]
    async fn checksum_mismatch_is_rejected() {
        let pool = open_mem().await;
        let v4 = mg(4, "mutable", "CREATE TABLE t (x INTEGER);");
        run_with(&pool, &[v4], DEFAULT_BASELINE).await.unwrap();
        let tampered = mg(4, "mutable", "CREATE TABLE t (x TEXT);");
        let err = run_with(&pool, &[tampered], DEFAULT_BASELINE)
            .await
            .unwrap_err();
        assert!(err.contains("checksum"), "错误信息: {err}");
    }

    #[tokio::test]
    async fn applied_version_missing_from_registry_is_rejected() {
        let pool = open_mem().await;
        let v4 = mg(4, "gone", "CREATE TABLE t (x INTEGER);");
        run_with(&pool, &[v4], DEFAULT_BASELINE).await.unwrap();
        // 新程序的注册表里没有 V4（降级场景）
        let err = run_with(&pool, &[], DEFAULT_BASELINE).await.unwrap_err();
        assert!(err.contains("V4") && err.contains("降级"), "错误信息: {err}");
    }

    #[tokio::test]
    async fn invalid_registry_is_rejected() {
        let pool = open_mem().await;
        // 版本不能等于基线
        let at_baseline = mg(DEFAULT_BASELINE, "x", "SELECT 1;");
        assert!(run_with(&pool, &[at_baseline], DEFAULT_BASELINE).await.is_err());

        // 必须严格递增
        let v5 = mg(5, "five", "SELECT 1;");
        let v4 = mg(4, "four", "SELECT 1;");
        assert!(run_with(&pool, &[v5, v4], DEFAULT_BASELINE).await.is_err());

        // 名称只能是小写 snake_case
        let bad_name = mg(4, "Bad-Name", "SELECT 1;");
        assert!(run_with(&pool, &[bad_name], DEFAULT_BASELINE).await.is_err());
    }
}
