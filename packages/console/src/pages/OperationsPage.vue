<script setup lang="ts">
import { onMounted, ref } from "vue";

import type { ConsoleApi, OperationDto } from "../api.js";
import { describeError, formatTime, operationStateView } from "../present.js";

const props = defineProps<{ api: ConsoleApi }>();

const operations = ref<OperationDto[]>([]);
const error = ref("");
const loading = ref(false);

// Pure views throw on names outside the machine; the page must not.
function badge(state: string): { tone: string; label: string } {
  try {
    return { ...operationStateView(state), label: state };
  } catch {
    return { tone: "other", label: state };
  }
}

async function load(): Promise<void> {
  loading.value = true;
  error.value = "";
  try {
    operations.value = (await props.api.listOperations()).operations;
  } catch (err) {
    error.value = describeError(err);
  } finally {
    loading.value = false;
  }
}

defineExpose({ load });

onMounted(() => {
  void load();
});
</script>

<template>
  <section>
    <h2>操作</h2>
    <p v-if="error" class="error">
      {{ error }}
    </p>
    <p v-if="loading" class="muted">
      加载中…
    </p>
    <table v-if="operations.length > 0">
      <thead>
        <tr>
          <th>操作</th>
          <th>类型</th>
          <th>状态</th>
          <th>创建时间</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="operation in operations" :key="operation.id">
          <td class="mono">
            {{ operation.id }}
          </td>
          <td>{{ operation.kind }}</td>
          <td>
            <span class="badge" :class="`tone-${badge(operation.state).tone}`">{{
              badge(operation.state).label
            }}</span>
          </td>
          <td>{{ formatTime(operation.created_at) }}</td>
        </tr>
      </tbody>
    </table>
    <p v-else-if="!loading" class="muted">
      暂无操作
    </p>
  </section>
</template>
