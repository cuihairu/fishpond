<script setup lang="ts">
import { computed, ref, shallowRef } from "vue";

import { ConsoleApi } from "./api.js";
import { describeError, permissionView } from "./present.js";
import AccountsPage from "./pages/AccountsPage.vue";
import EventsPage from "./pages/EventsPage.vue";
import OperationsPage from "./pages/OperationsPage.vue";
import SessionsPage from "./pages/SessionsPage.vue";
import StatusPage from "./pages/StatusPage.vue";

const TABS = [
  { id: "status", label: "状态" },
  { id: "accounts", label: "账户" },
  { id: "sessions", label: "会话" },
  { id: "operations", label: "操作" },
  { id: "events", label: "事件" },
] as const;

type TabId = (typeof TABS)[number]["id"];

const baseUrl = ref(sessionStorage.getItem("fp.console.url") ?? "http://127.0.0.1:8080");
const apiKey = ref(sessionStorage.getItem("fp.console.key") ?? "");
const scopesRaw = ref(sessionStorage.getItem("fp.console.scopes") ?? "");
const active = ref<TabId>("status");
const connectError = ref("");
// shallowRef: deep ref unwrapping would strip the class's private
// members and break the prop types of the pages below.
const api = shallowRef<ConsoleApi | undefined>(undefined);

// The console never persists the key beyond this browser's
// sessionStorage, never sends it anywhere but the configured base
// URL, and never prints it.
const scopes = computed(() => scopesRaw.value.split(/[\s,]+/).filter((scope) => scope.length > 0));
const permission = computed(() => permissionView(scopes.value));

function persist(): void {
  sessionStorage.setItem("fp.console.url", baseUrl.value);
  sessionStorage.setItem("fp.console.key", apiKey.value);
  sessionStorage.setItem("fp.console.scopes", scopesRaw.value);
}

async function connect(): Promise<void> {
  connectError.value = "";
  const candidate = new ConsoleApi(baseUrl.value, () =>
    apiKey.value === "" ? undefined : apiKey.value,
  );
  try {
    await candidate.health();
    api.value = candidate;
    persist();
  } catch (err) {
    connectError.value = describeError(err);
  }
}
</script>

<template>
  <header>
    <h1>Fishpond Console</h1>
    <form class="connect" @submit.prevent="connect">
      <input v-model="baseUrl" placeholder="控制面地址">
      <input
        v-model="apiKey"
        type="password"
        autocomplete="off"
        placeholder="API 密钥"
      >
      <input v-model="scopesRaw" placeholder="密钥 scopes（逗号分隔）">
      <button type="submit">
        连接
      </button>
      <span v-if="permission.missing.length > 0" class="muted">
        缺少：{{ permission.missing.join(", ") }}
      </span>
    </form>
    <p v-if="connectError" class="error">
      {{ connectError }}
    </p>
  </header>
  <nav>
    <button
      v-for="tab in TABS"
      :key="tab.id"
      type="button"
      :class="{ active: active === tab.id }"
      :disabled="api === undefined"
      @click="active = tab.id"
    >
      {{ tab.label }}
    </button>
  </nav>
  <main v-if="api !== undefined">
    <StatusPage v-if="active === 'status'" :api="api" :scopes="scopes" />
    <AccountsPage v-else-if="active === 'accounts'" :api="api" />
    <SessionsPage v-else-if="active === 'sessions'" :api="api" />
    <OperationsPage v-else-if="active === 'operations'" :api="api" />
    <EventsPage v-else-if="active === 'events'" :api="api" />
  </main>
  <main v-else>
    <p class="muted">
      先填写控制面地址并连接，再进入各管理页。
    </p>
  </main>
</template>
