<script setup lang="ts">
import { computed, onMounted, ref } from "vue";

import type { ConsoleApi, StatusSummary } from "../api.js";
import { describeError, permissionView } from "../present.js";

const props = defineProps<{ api: ConsoleApi; scopes: string[] }>();

const health = ref<{ status: string; service: string; version: string } | null>(null);
const summary = ref<StatusSummary | null>(null);
const error = ref("");

const permission = computed(() => permissionView(props.scopes));

async function load(): Promise<void> {
  error.value = "";
  try {
    const [h, s] = await Promise.all([props.api.health(), props.api.status()]);
    health.value = h;
    summary.value = s;
  } catch (err) {
    error.value = describeError(err);
  }
}

defineExpose({ load });

onMounted(() => {
  void load();
});
</script>

<template>
  <section>
    <h2>状态</h2>
    <p v-if="error" class="error">
      {{ error }}
    </p>
    <div v-if="health" class="cards">
      <div class="card">
        <div class="muted">
          服务
        </div>
        <div>{{ health.service }} {{ health.version }}</div>
      </div>
      <div class="card">
        <div class="muted">
          在线
        </div>
        <div class="num tone-healthy">
          {{ summary?.online ?? "—" }}
        </div>
      </div>
      <div class="card">
        <div class="muted">
          离线
        </div>
        <div class="num tone-warn">
          {{ summary?.offline ?? "—" }}
        </div>
      </div>
      <div class="card">
        <div class="muted">
          无法观测
        </div>
        <div class="num tone-observe">
          {{ summary?.unknown ?? "—" }}
        </div>
      </div>
      <div class="card">
        <div class="muted">
          停用
        </div>
        <div class="num tone-dead">
          {{ summary?.disabled ?? "—" }}
        </div>
      </div>
    </div>
    <div class="panel">
      <h3>当前密钥权限</h3>
      <p class="muted">
        scopes：{{ scopes.length > 0 ? scopes.join(", ") : "（未声明）" }}
      </p>
      <p>
        会话读 {{ permission.can.readSessions ? "✓" : "✗" }} · 会话写
        {{ permission.can.writeSessions ? "✓" : "✗" }} · 接管
        {{ permission.can.takeover ? "✓" : "✗" }}
      </p>
      <p v-if="permission.missing.length > 0" class="muted">
        缺少：{{ permission.missing.join(", ") }}
      </p>
    </div>
    <p class="muted">
      UNKNOWN 是观测结果，不代表健康与否；页面会以独立样式显示。
    </p>
  </section>
</template>
