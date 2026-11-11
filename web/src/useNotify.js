/* ==========================================================================
 * useNotify.js —— Vue 绑定层（薄）
 * -------------------------------------------------------------------------
 * 把 core 的纯状态机包成响应式引用，组件直接消费；不依赖 pinia / router。
 * 宿主若坚持用 pinia，自行包一层 store 即可（本项目宿主就是 10 行适配）。
 * ========================================================================== */
import { getCurrentScope, onScopeDispose, ref, shallowRef } from 'vue'
import { createNotifyQueue } from './core'

/**
 * @param {import('./core').NotifyQueueOptions} [options] 透传给 createNotifyQueue
 */
export function useNotify(options = {}) {
  const core = createNotifyQueue(options)

  const queue = shallowRef([])
  const current = shallowRef(null)
  const restCount = ref(0)
  const openTarget = shallowRef(null)

  function sync() {
    const s = core.snapshot()
    queue.value = s.queue
    current.value = s.current
    restCount.value = s.restCount
    openTarget.value = s.openTarget
  }

  const off = core.subscribe(sync)
  if (getCurrentScope()) onScopeDispose(off)
  sync()

  return {
    ingest: (snap) => core.ingest(snap),
    dismiss: (item) => core.dismiss(item),
    takeOpenTarget: () => {
      const t = core.takeOpenTarget()
      sync()
      return t
    },
    clear: () => core.clear(),
    queue,
    current,
    restCount,
    openTarget,
  }
}
