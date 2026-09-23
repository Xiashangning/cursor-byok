import assert from "node:assert/strict";
import test from "node:test";
import { parseOptions } from "./modelDefaults.ts";

test("parseOptions trims entries and removes empty comma-separated values", () => {
  assert.deepEqual(parseOptions(" low, , medium ,, high, "), ["low", "medium", "high"]);
});

test("parseOptions removes duplicates while preserving first occurrence order", () => {
  assert.deepEqual(parseOptions("high, low, high, medium, low"), ["high", "low", "medium"]);
});

test("parseOptions preserves casing and treats differently cased values as distinct", () => {
  assert.deepEqual(parseOptions("High, high, HIGH, High"), ["High", "high", "HIGH"]);
});

test("parseOptions returns no options for empty input", () => {
  for (const text of ["", "   ", ", ,,,\t"]) {
    assert.deepEqual(parseOptions(text), []);
  }
});

test("parseOptions parses successive input values without retaining removed options", () => {
  for (const [text, expected] of [
    ["200k", ["200k"]],
    ["200k,", ["200k"]],
    ["200k, 1", ["200k", "1"]],
    ["200k, 1m", ["200k", "1m"]],
    ["1m", ["1m"]],
    ["", []],
  ]) {
    assert.deepEqual(parseOptions(text), expected);
  }
});
