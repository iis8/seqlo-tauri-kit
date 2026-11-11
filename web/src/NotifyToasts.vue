<template>
  <Teleport to="body">
    <transition name="nt-pop">
      <div v-if="item" class="nt-wrap" role="status" aria-live="polite">
        <div class="nt-card" @mouseenter="hover = true" @mouseleave="hover = false">
          <span class="nt-dot" :class="item.kind" />
          <div class="nt-main" @click="onOpen">
            <div class="nt-title">{{ item.title }}</div>
            <div v-if="item.body" class="nt-body">{{ item.body }}</div>
          </div>
          <div class="nt-actions">
            <button v-if="canOpen" class="nt-link" @click.stop="onOpen">查看</button>
            <button class="nt-close" aria-label="关闭通知" @click.stop="onClose">×</button>
          </div>
        </div>
        <div v-if="restCount > 0" class="nt-rest">还有 {{ restCount }} 条 · 点击关闭逐条查看</div>
      </div>
    </transition>
  </Teleport>
</template>

<script setup>
/* 纯渲染：只认 item / restCount，跳哪里由宿主决定（emit open，宿主自行 router.push）。
 * 自动关闭计时在组件内：重要事件/摘要 8s，普通 5s；悬停暂停，移开重新计时。 */
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = defineProps({
  item: { type: Object, default: null },
  restCount: { type: Number, default: 0 },
  importantMs: { type: Number, default: 8000 },
  normalMs: { type: Number, default: 5000 },
})
const emit = defineEmits(['close', 'open'])

const hover = ref(false)
let timer = null

const canOpen = computed(
  () => !!(props.item && props.item.target && props.item.target.route),
)

function clearTimer() {
  if (timer) {
    clearTimeout(timer)
    timer = null
  }
}

function schedule() {
  clearTimer()
  if (!props.item) return
  const ms = props.item.important || props.item.kind === 'digest' ? props.importantMs : props.normalMs
  timer = setTimeout(() => {
    if (!hover.value) onClose()
  }, ms)
}

watch(() => props.item, () => { if (!hover.value) schedule() })
watch(hover, (v) => (v ? clearTimer() : schedule()))

function onClose() {
  emit('close', props.item)
}

function onOpen() {
  if (!props.item) return
  if (!canOpen.value) {
    onClose()
    return
  }
  emit('open', props.item)
}

function onKey(e) {
  if (e.key === 'Escape' && props.item) onClose()
}

onMounted(() => {
  document.addEventListener('keydown', onKey)
  schedule()
})
onBeforeUnmount(() => {
  clearTimer()
  document.removeEventListener('keydown', onKey)
})
</script>

<style scoped>
/* 颜色全部带兜底：宿主没引入主题 token 也能正常显示 */
.nt-wrap {
  position: fixed;
  top: 58px;
  left: 50%;
  transform: translateX(-50%);
  z-index: 9000;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  max-width: min(92vw, 560px);
  pointer-events: none;
}
.nt-card {
  pointer-events: auto;
  display: flex;
  align-items: flex-start;
  gap: 10px;
  width: max-content;
  max-width: 100%;
  padding: 12px 12px 12px 14px;
  background: var(--c-paper, #fff);
  color: var(--c-ink, #1f2329);
  border: 1px solid var(--c-hairline, #e3e6ea);
  border-radius: 10px;
  box-shadow: var(--shadow-pop, 0 8px 28px rgba(0, 0, 0, 0.16));
}
.nt-dot {
  flex: 0 0 auto;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  margin-top: 5px;
  background: var(--c-primary, #3370ff);
}
.nt-dot.digest {
  background: var(--c-accent, #ff8800);
}
.nt-dot.recent {
  background: var(--c-muted, #8a9099);
}
.nt-main {
  min-width: 0;
  cursor: pointer;
}
.nt-title {
  font-size: 13px;
  font-weight: 600;
  line-height: 1.4;
}
.nt-body {
  font-size: 12px;
  color: var(--c-muted, #8a9099);
  line-height: 1.5;
  margin-top: 2px;
  max-width: 420px;
  word-break: break-word;
}
.nt-actions {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 1px;
}
.nt-link {
  border: 0;
  background: none;
  color: var(--c-primary, #3370ff);
  font-size: 12px;
  cursor: pointer;
  padding: 2px 4px;
  border-radius: 4px;
}
.nt-link:hover {
  background: color-mix(in srgb, var(--c-primary, #3370ff) 12%, transparent);
}
.nt-close {
  border: 0;
  background: none;
  color: var(--c-muted, #8a9099);
  font-size: 16px;
  line-height: 1;
  cursor: pointer;
  padding: 2px 6px;
  border-radius: 4px;
}
.nt-close:hover {
  color: var(--c-ink, #1f2329);
  background: var(--c-hairline, #e3e6ea);
}
.nt-rest {
  pointer-events: none;
  font-size: 11px;
  color: var(--c-muted, #8a9099);
}
.nt-pop-enter-active,
.nt-pop-leave-active {
  transition: opacity 0.18s ease, transform 0.18s ease;
}
.nt-pop-enter-from,
.nt-pop-leave-to {
  opacity: 0;
  transform: translate(-50%, -6px);
}
</style>
