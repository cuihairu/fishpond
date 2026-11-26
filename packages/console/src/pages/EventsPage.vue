<script setup lang="ts">
import { onMounted, ref } from "vue";

import type { ConsoleApi, EventDto } from "../api.js";
import { describeError, formatTime } from "../present.js";

const props = defineProps<{ api: ConsoleApi }>();

const events = ref<EventDto[]>([]);
const cursor = ref<string | undefined>(undefined);
const error = ref("");
const loading = ref(false);

async function load(after?: string): Promise<void> {
  loading.value = true;
  error.value = "";
  try {
    const page = await props.api.listEvents(after);
    if (after === undefined) {
      events.value = page.events;
    } else {
      events.value = [...events.value, ...page.events];
    }
    cursor.value = page.next_cursor;
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
    <h2>事件</h2>
    <p v-if="error" class="error">
      {{ error }}
    </p>
    <p v-if="loading" class="muted">
      加载中…
    </p>
    <table v-if="events.length > 0">
      <thead>
        <tr>
          <th>seq</th>
          <th>类型</th>
          <th>账户</th>
          <th>时间</th>
          <th>载荷</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="event in events" :key="event.id">
          <td class="mono">
            {{ event.seq }}
          </td>
          <td class="mono">
            {{ event.event_type }}
          </td>
          <td class="mono">
            {{ event.account_id ?? "—" }}
          </td>
          <td>{{ formatTime(event.occurred_at) }}</td>
          <td class="mono muted">
            {{ event.payload === undefined || event.payload === null ? "—" : JSON.stringify(event.payload).slice(0, 120) }}
          </td>
        </tr>
      </tbody>
    </table>
    <p v-else-if="!loading" class="muted">
      暂无事件
    </p>
    <button v-if="cursor !== undefined" type="button" @click="load(cursor)">
      加载更多
    </button>
  </section>
</template>
