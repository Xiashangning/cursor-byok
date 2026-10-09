import assert from "node:assert/strict";
import test from "node:test";
import { AUTO_UPDATE_CHECK_INTERVAL_MS, createAutomaticUpdateCheck } from "../src/shared/store/automaticUpdateCheck.ts";

test("failed automatic checks remain throttled while manual checks bypass the interval", async () => {
  let time = 0;
  let calls = 0;
  const check = async () => { calls++; throw new Error("offline"); };
  const automatic = createAutomaticUpdateCheck(check, () => time);
  await assert.rejects(automatic(), /offline/);
  time = AUTO_UPDATE_CHECK_INTERVAL_MS - 1;
  assert.equal(await automatic(), null);
  assert.equal(calls, 1);
  await assert.rejects(check(), /offline/);
  assert.equal(calls, 2);
  time++;
  await assert.rejects(automatic(), /offline/);
  assert.equal(calls, 3);
});

test("successful automatic checks also consume the interval", async () => {
  const automatic = createAutomaticUpdateCheck(async () => "1.2.3", () => 0);
  assert.equal(await automatic(), "1.2.3");
  assert.equal(await automatic(), null);
});
