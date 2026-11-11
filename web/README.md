# @seqlo/tauri-notify

Tauri 2 桌面端**应用内通知浮层**：队列 / 摘要 / 兜底引擎 + Vue 组件。
与 Rust 侧 `vc-desktop-kit::notify` 分工明确：**Rust 决定发不发系统通知，前端只负责把
「前台事件」排成浮层**，两边不重复弹。

## 安装

```bash
npm i @seqlo/tauri-notify            # 或本地 file: 依赖
```

- `vue` 是 peerDependency（`^3.4`）；不依赖 pinia / vue-router / element-plus。
- 颜色走 `--c-*` CSS 变量，缺失时自带兜底色。

## 组成

| 导出 | 说明 |
|---|---|
| `createNotifyQueue(opts)` | 纯状态机（无 Vue），可在 Node 环境直接单测 |
| `useNotify(opts)` | Vue 绑定：`{ ingest, dismiss, takeOpenTarget, clear, current, restCount, queue, openTarget }` |
| `NotifyToasts` | 纯渲染组件：`props { item, restCount }`，`emits { close, open }` |

## 快照契约（宿主喂给引擎的数据）

```js
{
  pending: [{ id, title, body, important?, target?: { route } }],
  digest: { count, titles: [] } | null,
  recent: [{ ts /* 秒 */, title, body, target }],
  open_target: { route } | null   // 系统通知被点击后后端暂存的目标
}
```

规则：pending 按 id 去重（上限 200）；digest 按签名去重；recent 仅 10 分钟内、
且不与浮层/摘要叠加时兜底；`open_target` 出现即通过 `consume(['__open__'])` 回收。

## 接入示例（最简，无 pinia）

```js
import { useNotify } from '@seqlo/tauri-notify'
import { NotifyToasts } from '@seqlo/tauri-notify'

const notify = useNotify({
  consume: (ids) => invoke('notify_consume', { ids }),   // 必填：回收后端状态
  digestTarget: { route: '/alerts' },                     // 业务路由由宿主决定
})

// 宿主的轮询（示例：5 秒一次）
setInterval(async () => {
  const d = await invoke('get_stats', { days: 1 })
  notify.ingest(d?.notify)
  const t = notify.takeOpenTarget()
  if (t?.route) router.push(t.route).catch(() => router.push('/alerts'))
}, 5000)
```

```vue
<NotifyToasts
  :item="notify.current.value"
  :rest-count="notify.restCount.value"
  @close="notify.dismiss($event)"
  @open="(it) => { notify.dismiss(it); router.push(it.target.route) }"
/>
```

## 用 pinia（可选）

```js
export const useNotifyStore = defineStore('notify', {
  state: () => ({
    queue: [],
    openTarget: null,
    _core: markRaw(createNotifyQueue({ consume, digestTarget: { route: '/alerts' } })),
  }),
  getters: {
    current: (s) => s.queue[0] || null,
    restCount: (s) => Math.max(0, s.queue.length - 1),
  },
  actions: {
    _sync() { const s = this._core.snapshot(); this.queue = s.queue; this.openTarget = s.openTarget },
    ingest(snap) { this._core.ingest(snap); this._sync() },
    dismiss(item) { this._core.dismiss(item); this._sync() },
    takeOpenTarget() { const t = this._core.takeOpenTarget(); this._sync(); return t },
    clear() { this._core.clear(); this._sync() },
  },
})
```

## 测试

```bash
npm install
npm test        # core 8 例 + 组件 4 例
```
