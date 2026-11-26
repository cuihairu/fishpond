<script setup lang="ts">
import { onMounted, ref } from "vue";

import type { ConsoleApi, GrantDto, SessionDto, TakeoverContext } from "../api.js";
import { describeError, formatTime, sessionActions, sessionStateView } from "../present.js";

const props = defineProps<{ api: ConsoleApi }>();

const sessions = ref<SessionDto[]>([]);
const error = ref("");
const loading = ref(false);
const credId = ref("");

// Takeover panel state (one open panel at a time).
const openId = ref<string | null>(null);
const grants = ref<GrantDto[]>([]);
const operator = ref("");
const ttl = ref("900000");
const issued = ref<{ grant_id: string; token: string; expires_at: number } | null>(null);

// Possession-path verify box (works without a configured key).
const verifyToken = ref("");
const verified = ref<TakeoverContext | null>(null);
const verifyError = ref("");

function badge(state: string): { tone: string; label: string } {
  try {
    return { ...sessionStateView(state), label: state };
  } catch {
    return { tone: "other", label: state };
  }
}

async function load(): Promise<void> {
  loading.value = true;
  error.value = "";
  try {
    sessions.value = (await props.api.listSessions()).sessions;
  } catch (err) {
    error.value = describeError(err);
  } finally {
    loading.value = false;
  }
}

async function loadGrants(id: string): Promise<void> {
  try {
    grants.value = (await props.api.listTakeoverGrants(id)).grants;
  } catch (err) {
    error.value = describeError(err);
  }
}

function open(session: SessionDto): void {
  openId.value = session.id;
  issued.value = null;
  grants.value = [];
  void loadGrants(session.id);
}

async function issueFor(session: SessionDto): Promise<void> {
  error.value = "";
  try {
    const ttlMs = Number(ttl.value);
    issued.value = await props.api.issueTakeover(session.id, {
      operator: operator.value.trim() === "" ? "console-operator" : operator.value.trim(),
      ttl_ms: Number.isFinite(ttlMs) ? ttlMs : undefined,
    });
    await loadGrants(session.id);
    await load();
  } catch (err) {
    error.value = describeError(err);
  }
}

async function release(session: SessionDto, grant: GrantDto): Promise<void> {
  error.value = "";
  try {
    await props.api.releaseTakeover(session.id, grant.id);
    await loadGrants(session.id);
    await load();
  } catch (err) {
    error.value = describeError(err);
  }
}

async function doTransition(session: SessionDto, action: string): Promise<void> {
  error.value = "";
  try {
    // The takeover flow is the grant route; everything else is a
    // plain transition. authorize carries the credential to bind.
    const body: { action: string; credential_id?: string } = { action };
    if (action === "authorize") body.credential_id = credId.value.trim();
    await props.api.transitionSession(session.id, body);
    credId.value = "";
    await load();
  } catch (err) {
    error.value = describeError(err);
  }
}

function onAction(session: SessionDto, action: string): void {
  if (action === "take-over") {
    open(session);
    void issueFor(session);
    return;
  }
  void doTransition(session, action);
}

async function copyToken(): Promise<void> {
  if (issued.value === null) return;
  try {
    await navigator.clipboard.writeText(issued.value.token);
  } catch {
    // Clipboard can be unavailable; the token stays visible instead.
  }
}

async function verify(): Promise<void> {
  verifyError.value = "";
  verified.value = null;
  try {
    verified.value = await props.api.verifyTakeover(verifyToken.value.trim());
    verifyToken.value = "";
  } catch (err) {
    verifyError.value = describeError(err);
  }
}

defineExpose({ load });

onMounted(() => {
  void load();
});
</script>

<template>
  <section>
    <h2>会话</h2>
    <p v-if="error" class="error">
      {{ error }}
    </p>
    <p v-if="loading" class="muted">
      加载中…
    </p>
    <table v-if="sessions.length > 0">
      <thead>
        <tr>
          <th>会话</th>
          <th>账户</th>
          <th>平台</th>
          <th>状态</th>
          <th>凭据</th>
          <th>更新时间</th>
          <th>操作</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="session in sessions" :key="session.id">
          <td class="mono">
            {{ session.id }}
          </td>
          <td class="mono">
            {{ session.account_id }}
          </td>
          <td>{{ session.platform }}</td>
          <td>
            <span class="badge" :class="`tone-${badge(session.state).tone}`">{{
              badge(session.state).label
            }}</span>
            <span v-if="session.state_note" class="note">{{ session.state_note }}</span>
          </td>
          <td class="mono">
            {{ session.credential_id ?? "—" }}
          </td>
          <td>{{ formatTime(session.updated_at) }}</td>
          <td>
            <button
              v-for="action in sessionActions(session.state)"
              :key="action"
              type="button"
              @click="onAction(session, action)"
            >
              {{ action }}
            </button>
            <button type="button" @click="open(session)">
              接管面板
            </button>
          </td>
        </tr>
      </tbody>
    </table>
    <p v-else-if="!loading" class="muted">
      暂无会话
    </p>

    <div v-if="openId !== null" class="panel">
      <h3>接管面板：{{ openId }}</h3>
      <form class="actions" @submit.prevent="issueFor(sessions.find((s) => s.id === openId)!)">
        <input v-model="operator" placeholder="操作者标识">
        <input v-model="ttl" class="mono" placeholder="TTL 毫秒（≤900000）">
        <button type="submit">
          签发接管凭据
        </button>
      </form>
      <div v-if="issued !== null" class="token-banner">
        <strong>接管令牌（仅显示这一次）：</strong>
        <span class="mono">{{ issued.token }}</span>
        <button type="button" @click="copyToken()">
          复制
        </button>
        <p class="muted">
          服务器只存哈希；关闭页面后无法再次查看。有效期至 {{ formatTime(issued.expires_at) }}。
        </p>
      </div>
      <table v-if="grants.length > 0">
        <thead>
          <tr>
            <th>grant</th>
            <th>操作者</th>
            <th>令牌尾提示</th>
            <th>签发</th>
            <th>到期</th>
            <th>状态</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="grant in grants" :key="grant.id">
            <td class="mono">
              {{ grant.id }}
            </td>
            <td>{{ grant.operator }}</td>
            <td class="mono">
              {{ grant.token_hint }}
            </td>
            <td>{{ formatTime(grant.created_at) }}</td>
            <td>{{ formatTime(grant.expires_at) }}</td>
            <td>
              <span v-if="grant.revoked_at !== undefined" class="badge tone-dead">已吊销</span>
              <span v-else class="badge tone-busy">有效</span>
            </td>
            <td>
              <button v-if="grant.revoked_at === undefined" type="button" @click="release(sessions.find((s) => s.id === openId)!, grant)">
                释放并恢复
              </button>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="muted">
        该会话暂无接管记录。
      </p>
    </div>

    <div class="panel">
      <h3>接管凭据验证（无需密钥）</h3>
      <form class="actions" @submit.prevent="verify">
        <input v-model="verifyToken" class="mono" placeholder="fp_tk_…">
        <button type="submit">
          验证
        </button>
      </form>
      <p v-if="verifyError" class="error">
        {{ verifyError }}
      </p>
      <div v-if="verified !== null" class="panel">
        <p class="mono">
          grant：{{ verified.grant_id }}
        </p>
        <p class="mono">
          租户：{{ verified.tenant }} · 账户：{{ verified.account }} · 会话：{{ verified.session }}
        </p>
        <p class="mono">
          操作者：{{ verified.operator }} · 到期：{{ formatTime(verified.expires_at) }}
        </p>
      </div>
    </div>
  </section>
</template>
