// @vitest-environment happy-dom
// F035: the mounted shell — tabs, the UNKNOWN observational badge,
// the takeover panel, and the permission strip must render from the
// domain rules, not from ad-hoc strings.

import { flushPromises, mount } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import App from "../src/App.vue";

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function fetchRouter(url: string): Response {
  if (url.endsWith("/healthz")) {
    return jsonResponse(200, { status: "ok", service: "fishpond-server", version: "9.9.9-test" });
  }
  if (url.endsWith("/status")) {
    return jsonResponse(200, { online: 1, offline: 0, unknown: 1, disabled: 0, total: 2 });
  }
  if (url.endsWith("/accounts")) {
    return jsonResponse(200, {
      accounts: [
        { id: "acc-ok", platform: "mock", state: "ONLINE", updated_at: 1_000 },
        {
          id: "acc-unknown",
          platform: "mock",
          state: "UNKNOWN",
          state_note: "probe timed out",
          updated_at: 2_000,
        },
      ],
    });
  }
  if (url.endsWith("/sessions")) {
    return jsonResponse(200, {
      sessions: [
        {
          id: "sess-2",
          account_id: "acc-ok",
          platform: "mock",
          state: "TAKEN_OVER",
          created_at: 1_000,
          updated_at: 3_000,
        },
      ],
    });
  }
  if (url.includes("/takeover") && url.includes("/sessions/")) {
    return jsonResponse(200, {
      grants: [
        {
          id: "tg-1",
          operator: "op",
          token_hint: "abcd",
          created_at: 1_000,
          expires_at: 9_000,
        },
      ],
    });
  }
  if (url.endsWith("/operations")) {
    return jsonResponse(200, { operations: [] });
  }
  if (url.endsWith("/events")) {
    return jsonResponse(200, { events: [], next_cursor: null });
  }
  throw new Error(`unexpected fetch: ${url}`);
}

async function mountConnected(): Promise<ReturnType<typeof mount<typeof App>>> {
  sessionStorage.setItem("fp.console.url", "http://control.test");
  sessionStorage.setItem("fp.console.key", "key-secret");
  sessionStorage.setItem("fp.console.scopes", "sessions:read,sessions:takeover");
  const wrapper = mount(App);
  await flushPromises();
  await wrapper.find("form.connect button").trigger("submit");
  await flushPromises();
  return wrapper;
}

async function openTab(wrapper: ReturnType<typeof mount<typeof App>>, label: string): Promise<void> {
  const tab = wrapper.findAll("nav button").find((node) => node.text() === label);
  expect(tab, `tab ${label} exists`).toBeDefined();
  await tab!.trigger("click");
  await flushPromises();
}

describe("mounted console shell", () => {
  const fetchMock = vi.fn().mockImplementation(fetchRouter);

  beforeEach(() => {
    vi.stubGlobal("fetch", fetchMock);
    sessionStorage.clear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("connects, shows health, and lists five tabs", async () => {
    const wrapper = await mountConnected();
    expect(wrapper.text()).toContain("fishpond-server");
    expect(wrapper.findAll("nav button")).toHaveLength(5);
    expect(wrapper.find("nav button.active").text()).toBe("状态");
    wrapper.unmount();
  });

  it("renders UNKNOWN as an observational badge with its note", async () => {
    const wrapper = await mountConnected();
    await openTab(wrapper, "账户");
    const unknownBadge = wrapper.find(".badge.tone-observe");
    expect(unknownBadge.exists()).toBe(true);
    expect(unknownBadge.text()).toBe("UNKNOWN");
    expect(wrapper.text()).toContain("probe timed out");
    // An UNKNOWN account offers no UNKNOWN self-transition button.
    const row = unknownBadge.element.closest("tr")!;
    const buttons = Array.from(row.querySelectorAll("td:last-child button")).map(
      (node) => node.textContent?.trim() ?? "",
    );
    expect(buttons).toEqual(["ONLINE", "OFFLINE", "DISABLED"]);
    wrapper.unmount();
  });

  it("renders TAKEN_OVER as busy with only its legal actions", async () => {
    const wrapper = await mountConnected();
    await openTab(wrapper, "会话");
    const badge = wrapper.find(".badge.tone-busy");
    expect(badge.exists()).toBe(true);
    expect(badge.text()).toBe("TAKEN_OVER");
    const row = badge.element.closest("tr")!;
    const buttons = Array.from(row.querySelectorAll("td:last-child button")).map(
      (node) => node.textContent?.trim() ?? "",
    );
    expect(buttons).toEqual(["resume", "expire", "revoke", "接管面板"]);
    wrapper.unmount();
  });

  it("opens the takeover panel and lists the live grant", async () => {
    const wrapper = await mountConnected();
    await openTab(wrapper, "会话");
    await wrapper.findAll("button").find((node) => node.text() === "接管面板")!.trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("tg-1");
    expect(wrapper.text()).toContain("abcd");
    expect(wrapper.text()).toContain("有效");
    wrapper.unmount();
  });

  it("names the missing scopes of the configured key", async () => {
    const wrapper = await mountConnected();
    expect(wrapper.text()).toContain("缺少：sessions:write");
    await openTab(wrapper, "状态");
    expect(wrapper.text()).toContain("会话写 ✗");
    expect(wrapper.text()).toContain("接管 ✓");
    wrapper.unmount();
  });
});
