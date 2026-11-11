import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { nextTick } from 'vue'
import NotifyToasts from '../src/NotifyToasts.vue'

function click(selector) {
  const el = document.querySelector(selector)
  expect(el, `应存在 ${selector}`).toBeTruthy()
  el.dispatchEvent(new MouseEvent('click', { bubbles: true }))
}

describe('NotifyToasts（纯渲染）', () => {
  it('无 item 时不渲染', () => {
    const w = mount(NotifyToasts, { props: { item: null }, attachTo: document.body })
    expect(document.querySelector('.nt-wrap')).toBeNull()
    w.unmount()
  })

  it('渲染标题/正文/剩余条数，close 与 open 分别 emit', async () => {
    const item = { id: 'n1', kind: 'float', title: '通知标题', body: '通知正文', target: { route: '/x' } }
    const w = mount(NotifyToasts, { props: { item, restCount: 2 }, attachTo: document.body })

    expect(document.body.textContent).toContain('通知标题')
    expect(document.body.textContent).toContain('通知正文')
    expect(document.body.textContent).toContain('还有 2 条')

    click('.nt-close')
    await nextTick()
    expect(w.emitted('close')?.[0]?.[0]).toEqual(item)

    click('.nt-link')
    await nextTick()
    expect(w.emitted('open')?.[0]?.[0]).toEqual(item)

    w.unmount()
  })

  it('无 target 时不渲染「查看」，点正文等同关闭', async () => {
    const item = { id: 'n2', kind: 'recent', title: '无目标' }
    const w = mount(NotifyToasts, { props: { item }, attachTo: document.body })

    expect(document.querySelector('.nt-link')).toBeNull()
    click('.nt-main')
    await nextTick()
    expect(w.emitted('close')).toBeTruthy()

    w.unmount()
  })

  it('Esc 触发 close', async () => {
    const item = { id: 'n3', kind: 'digest', title: '摘要' }
    const w = mount(NotifyToasts, { props: { item }, attachTo: document.body })

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    await nextTick()
    expect(w.emitted('close')).toBeTruthy()

    w.unmount()
  })
})
