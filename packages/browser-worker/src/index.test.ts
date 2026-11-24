import { describe, expect, it } from "vitest";
import { createBrowserWorker } from "./index";

describe("createBrowserWorker", () => {
  it("returns the placeholder descriptor", () => {
    expect(createBrowserWorker({ id: "w1" })).toBe("browser-worker:w1:placeholder");
  });
});
