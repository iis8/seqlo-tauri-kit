/* ==========================================================================
 * core.js —— 应用内通知浮层的纯状态机（无 Vue / 无 pinia / 无 router）
 * -------------------------------------------------------------------------
 * 输入：后端快照 { pending, digest, recent, open_target }（由宿主按自己的
 *       轮询节奏喂进来，通常是 get_stats 那一次请求顺手带回的 notify 字段）。
 * 输出：待展示队列 + 待跳转目标。
 *
 * 规则（与 Rust 侧 notify 引擎互补，互不重复）：
 *   1. 前端只负责「前台事件排成浮层」，后台事件由 Rust 直接发系统通知；
 *   2. 同一快照的 pending 按 id 去重，消费过的 id 不再入队（_seen 上限 200）；
 *   3. 后台事件合并成一条摘要（digest），同签名不重复弹；
 *   4. 系统通知点击回调拿不到时，用 10 分钟内最近一条兜底，且不与浮层/摘要叠加；
 *   5. 用户看过/点过之后调用 consume(ids) 回收后端状态，避免重复弹。
 *
 * 所有外部依赖通过参数注入，故本文件可在 Node 环境直接单测。
 * ========================================================================== */

const PENDING_CAP = 200
const RECENT_WINDOW_MS = 10 * 60 * 1000

/**
 * @param {Object} [opts]
 * @param {(ids: string[]) => void} [opts.consume] 回收后端待弹状态（默认 no-op）
 * @param {() => number} [opts.now] 时钟（测试可注入）
 * @param {Object|null} [opts.digestTarget] 摘要条的点击目标（业务路由，宿主传入；默认无）
 * @param {(count: number, titles: string[]) => string} [opts.digestTitle] 摘要标题模板
 * @param {(title: string) => string} [opts.recentTitle] recent 兜底标题模板
 */
export function createNotifyQueue({
  consume = () => {},
  now = () => Date.now(),
  digestTarget = null,
  digestTitle = (count) => `你不在时发生了 ${count} 件事`,
  recentTitle = (title) => `刚才的通知：${title}`,
} = {}) {
  /** @type {Array<Object>} */
  const queue = []
  let openTarget = null
  /** @type {string[]} */
  let seen = []
  let seenRecentTs = 0
  let digestKey = ''
  /** @type {Array<() => void>} */
  let listeners = []

  const notify = () => {
    for (const l of listeners) l()
  }

  /** 只读快照；调用方不要直接改 queue 数组。 */
  const snapshot = () => ({
    queue: queue.slice(),
    openTarget,
    current: queue[0] || null,
    restCount: Math.max(0, queue.length - 1),
  })

  /** 订阅状态变化，返回取消函数。 */
  function subscribe(fn) {
    listeners.push(fn)
    return () => {
      listeners = listeners.filter((l) => l !== fn)
    }
  }

  /** 由宿主喂入一份后端快照。 */
  function ingest(snap) {
    if (!snap || typeof snap !== 'object') return
    let changed = false

    const pending = Array.isArray(snap.pending) ? snap.pending : []
    for (const p of pending) {
      const id = String(p.id || '')
      if (!id || seen.includes(id)) continue
      seen.push(id)
      if (seen.length > PENDING_CAP) seen.shift()
      queue.push({ ...p, kind: 'float' })
      changed = true
    }

    // 后台事件合并摘要：一次回来只补一条，不逐条补。
    const dg = snap.digest
    if (dg && Number(dg.count) > 0) {
      const titles = Array.isArray(dg.titles) ? dg.titles : []
      const key = `digest:${dg.count}:${titles.join('|')}`
      if (key !== digestKey) {
        digestKey = key
        queue.push({
          id: '__digest__',
          kind: 'digest',
          title: digestTitle(Number(dg.count), titles),
          body: titles.join('、') || '点击查看告警中心',
          target: digestTarget,
        })
        changed = true
      }
    }

    // 系统通知点击回调不可用时的兜底：10 分钟内的最近一条。
    const recent = Array.isArray(snap.recent) ? snap.recent : []
    const last = recent[recent.length - 1]
    // 已有浮层/摘要时不再叠加兜底，保证「回前台只补一条」。
    const hasImmediate = queue.length > 0 || (dg && Number(dg.count) > 0)
    if (last && !snap.open_target && !hasImmediate) {
      const ts = Number(last.ts || 0) * 1000
      if (ts && now() - ts <= RECENT_WINDOW_MS && ts !== seenRecentTs) {
        seenRecentTs = ts
        queue.push({
          id: '__recent__',
          kind: 'recent',
          title: recentTitle(last.title || ''),
          body: last.body || '',
          target: last.target || null,
        })
        changed = true
      }
    }

    // 系统通知被点击：交给宿主跳转（后端已把目标暂存）。
    if (snap.open_target) {
      openTarget = snap.open_target
      consume(['__open__'])
      changed = true
    }

    if (changed) notify()
  }

  /** 关闭指定一条（默认队首）并回收后端状态。 */
  function dismiss(item = null) {
    const it = item || queue[0]
    if (!it) return
    const idx = queue.findIndex((x) => x.id === it.id)
    if (idx >= 0) queue.splice(idx, 1)

    const ids = []
    if (it.id === '__digest__') {
      ids.push('__digest__')
      digestKey = ''
    } else if (it.id === '__recent__') {
      ids.push('__recent__')
    } else if (it.id) {
      ids.push(it.id)
    }
    if (ids.length) consume(ids)
    notify()
  }

  /** 取走并清空待跳转目标（宿主消费后自行 router.push）。 */
  function takeOpenTarget() {
    const t = openTarget
    openTarget = null
    return t
  }

  function clear() {
    queue.length = 0
    openTarget = null
    seen = []
    seenRecentTs = 0
    digestKey = ''
    notify()
  }

  return { ingest, dismiss, takeOpenTarget, clear, snapshot, subscribe }
}
