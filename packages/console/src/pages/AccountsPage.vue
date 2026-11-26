<script setup lang="ts">
import { onMounted, ref } from "vue";

import type { AccountDto, ConsoleApi } from "../api.js";
import { accountActions, accountStateView, describeError, formatTime } from "../present.js";

const props = defineProps<{ api: ConsoleApi }>();

const accounts = ref<AccountDto[]>([]);
const error = ref("");
const loading = ref(false);
const newId = ref("");
const newPlatform = ref("mock");
const noteFor = ref<string | null>(null);
const noteText = ref("");

// Pure views throw on names outside the machine; the page must not.
function badge(state: string): { tone: string; label: string } {
  try {
    return { ...accountStateView(state), label: state };
  } catch {
    return { tone: "other", label: state };
  }
}

async function load(): Promise<void> {
  loading.value = true;
  error.value = "";
  try {
    accounts.value = (await props.api.listAccounts()).accounts;
  } catch (err) {
    error.value = describeError(err);
  } finally {
    loading.value = false;
  }
}

async function register(): Promise<void> {
  error.value = "";
  try {
    await props.api.registerAccount({ id: newId.value.trim(), platform: newPlatform.value.trim() });
    newId.value = "";
    await load();
  } catch (err) {
    error.value = describeError(err);
  }
}

async function applyState(account: AccountDto, target: string, note?: string): Promise<void> {
  error.value = "";
  try {
    await props.api.setAccountState(account.id, { state: target, note });
    noteFor.value = null;
    noteText.value = "";
    await load();
  } catch (err) {
    error.value = describeError(err);
  }
}

function onAction(account: AccountDto, target: string): void {
  // UNKNOWN is an observation: the domain requires a note with it.
  if (target === "UNKNOWN") {
    noteFor.value = account.id;
    noteText.value = "";
    return;
  }
  void applyState(account, target);
}

defineExpose({ load });

onMounted(() => {
  void load();
});
</script>

<template>
  <section>
    <h2>账户</h2>
    <form class="actions" @submit.prevent="register">
      <input v-model="newId" placeholder="账户 id" required>
      <input v-model="newPlatform" placeholder="平台" required>
      <button type="submit">
        注册账户
      </button>
    </form>
    <p v-if="error" class="error">
      {{ error }}
    </p>
    <p v-if="loading" class="muted">
      加载中…
    </p>
    <table v-if="accounts.length > 0">
      <thead>
        <tr>
          <th>账户</th>
          <th>平台</th>
          <th>状态</th>
          <th>更新时间</th>
          <th>操作</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="account in accounts" :key="account.id">
          <td class="mono">
            {{ account.id }}
          </td>
          <td>{{ account.platform }}</td>
          <td>
            <span class="badge" :class="`tone-${badge(account.state).tone}`">{{
              badge(account.state).label
            }}</span>
            <span v-if="account.state_note" class="note">{{ account.state_note }}</span>
          </td>
          <td>{{ formatTime(account.updated_at) }}</td>
          <td>
            <template v-if="noteFor === account.id">
              <input v-model="noteText" placeholder="UNKNOWN 观测备注">
              <button type="button" @click="applyState(account, 'UNKNOWN', noteText || '状态无法确认')">
                确认
              </button>
              <button type="button" @click="noteFor = null">
                取消
              </button>
            </template>
            <template v-else>
              <button
                v-for="target in accountActions(account.state)"
                :key="target"
                type="button"
                @click="onAction(account, target)"
              >
                {{ target }}
              </button>
            </template>
          </td>
        </tr>
      </tbody>
    </table>
    <p v-else-if="!loading" class="muted">
      暂无账户
    </p>
  </section>
</template>
