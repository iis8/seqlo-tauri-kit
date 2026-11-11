import { describe, expect, it, vi } from 'vitest'
import { createNotifyQueue } from '../src/core'

describe('notify core（浮层队列 / 摘要 / 兜底）', () => {
  it('同一快照的 pending 只入队一次，消费后不再回弹', () => {
    const consume = vi.fn()
    const q = createNotifyQueue({ consume })
    const snap = {
      pending: [{ id: 'n0', title: 'A' }, { id: 'n1', title: 'B' }],
      digest: null,
      recent: [],
    }
    q.ingest(snap)
    expect(q.snapshot().queue.map((x) => x.id)).toEqual(['n0', 'n1'])

    q.ingest(snap)
    expect(q.snapshot().queue.length).toBe(2)

    q.dismiss()
    expect(q.snapshot().queue.map((x) => x.id)).toEqual(['n1'])
    expect(consume).toHaveBeenCalledWith(['n0'])
  })

  it('后台摘要只补一条，且同签名不重复弹', () => {
    const q = createNotifyQueue()
    const snap = { pending: [], digest: { count: 3, titles: ['A', 'B', 'C'] }, recent: [] }
    q.ingest(snap)
    const first = q.snapshot().queue[0]
    expect(first.id).toBe('__digest__')
    expect(first.title).toContain('3')

    const len = q.snapshot().queue.length
    q.ingest(snap)
    expect(q.snapshot().queue.length).toBe(len)
  })

  it('摘要条走宿主注入的跳转目标（默认无目标）', () => {
    const q = createNotifyQueue({ digestTarget: { route: '/alerts' } })
    q.ingest({ pending: [], digest: { count: 2, titles: ['X'] }, recent: [] })
    expect(q.snapshot().queue[0].target).toEqual({ route: '/alerts' })

    const plain = createNotifyQueue()
    plain.ingest({ pending: [], digest: { count: 2, titles: ['X'] }, recent: [] })
    expect(plain.snapshot().queue[0].target).toBeNull()
  })

  it('有点击目标时优先跳转，不再叠加最近通知兜底', () => {
    const consume = vi.fn()
    const q = createNotifyQueue({ consume })
    q.ingest({
      pending: [],
      digest: null,
      recent: [{ ts: Math.floor(Date.now() / 1000), title: '最近一条' }],
      open_target: { route: '/tasks' },
    })
    expect(q.snapshot().queue.length).toBe(0)
    expect(q.snapshot().openTarget).toEqual({ route: '/tasks' })
    expect(q.takeOpenTarget()).toEqual({ route: '/tasks' })
    expect(q.snapshot().openTarget).toBeNull()
    expect(consume).toHaveBeenCalledWith(['__open__'])
  })

  it('拿不到点击回调时，10 分钟内的最近一条作为兜底', () => {
    const q = createNotifyQueue()
    const ts = Math.floor(Date.now() / 1000)
    q.ingest({ pending: [], digest: null, recent: [{ ts, title: '刚才的通知' }], open_target: null })
    expect(q.snapshot().queue[0].id).toBe('__recent__')
    expect(q.snapshot().queue[0].title).toContain('刚才的通知')
  })

  it('超过 10 分钟的 recent 不再兜底', () => {
    const q = createNotifyQueue()
    const ts = Math.floor((Date.now() - 11 * 60 * 1000) / 1000)
    q.ingest({ pending: [], digest: null, recent: [{ ts, title: '很久以前' }], open_target: null })
    expect(q.snapshot().queue.length).toBe(0)
  })

  it('dismiss 摘要后同一签名可以重新入队', () => {
    const q = createNotifyQueue()
    const snap = { pending: [], digest: { count: 1, titles: ['A'] }, recent: [] }
    q.ingest(snap)
    q.dismiss()
    q.ingest(snap)
    expect(q.snapshot().queue.length).toBe(1)
  })

  it('clear 清空队列与去重记录', () => {
    const q = createNotifyQueue()
    q.ingest({ pending: [{ id: 'a', title: 'A' }], digest: null, recent: [] })
    q.clear()
    expect(q.snapshot().queue.length).toBe(0)
    q.ingest({ pending: [{ id: 'a', title: 'A' }], digest: null, recent: [] })
    expect(q.snapshot().queue.length).toBe(1)
  })
})
