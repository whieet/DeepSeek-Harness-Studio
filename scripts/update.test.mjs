import test from "node:test";
import assert from "node:assert/strict";

import { compareVersions, satisfiesRange, parseVersion } from "./update.mjs";

test("compareVersions: 预发布 < 正式发布", () => {
  assert.equal(compareVersions("0.1.1-rc.2", "0.1.1"), -1);
  assert.equal(compareVersions("0.1.1", "0.1.1-rc.2"), 1);
});

test("compareVersions: 预发布标识数字段按数值比较", () => {
  assert.equal(compareVersions("0.1.1-rc.2", "0.1.1-rc.10"), -1);
  assert.equal(compareVersions("1.0.0-2", "1.0.0-10"), -1);
});

test("compareVersions: 逐段数值比较", () => {
  assert.equal(compareVersions("0.2.0", "0.1.9"), 1);
  assert.equal(compareVersions("1.0.0", "1.0.0"), 0);
  assert.equal(compareVersions("0.10.0", "0.9.0"), 1);
});

test("compareVersions: semver 预发布点分规则", () => {
  assert.equal(compareVersions("1.0.0-alpha", "1.0.0-alpha.1"), -1);
  assert.equal(compareVersions("1.0.0-alpha.1", "1.0.0-beta"), -1);
  assert.equal(compareVersions("1.0.0-beta", "1.0.0"), -1);
});

test("compareVersions: 忽略前导 v；非法输入返回 null", () => {
  assert.equal(compareVersions("v1.2.3", "1.2.3"), 0);
  assert.equal(compareVersions("abc", "1.0.0"), null);
  assert.equal(parseVersion("not-a-version"), null);
  assert.equal(parseVersion("1.2"), null);
});

test("satisfiesRange: engines 常见写法", () => {
  assert.equal(satisfiesRange("24.20.0", ">=20"), true);
  assert.equal(satisfiesRange("18.0.0", ">=20"), false);
  assert.equal(satisfiesRange("24.20.0", ">=20.18.0"), true);
  assert.equal(satisfiesRange("24.20.0", "^20 || ^22 || ^24"), true);
  assert.equal(satisfiesRange("25.0.0", "^24"), false);
  assert.equal(satisfiesRange("24.20.0", ">=22 <25"), true);
  assert.equal(satisfiesRange("26.0.0", ">=22 <25"), false);
});

test("satisfiesRange: caret 与 tilde", () => {
  assert.equal(satisfiesRange("20.5.0", "^20.5"), true);
  assert.equal(satisfiesRange("21.0.0", "^20.5"), false);
  assert.equal(satisfiesRange("22.1.5", "~22.1"), true);
  assert.equal(satisfiesRange("22.2.0", "~22.1"), false);
});

test("satisfiesRange: x-range 与通配", () => {
  assert.equal(satisfiesRange("3.0.0", "*"), true);
  assert.equal(satisfiesRange("1.9.9", "1.x"), true);
  assert.equal(satisfiesRange("2.0.0", "1.x"), false);
  assert.equal(satisfiesRange("1.2.9", "1.2.x"), true);
  assert.equal(satisfiesRange("1.3.0", "1.2.x"), false);
});

test("satisfiesRange: 预发布守卫（semver 规则）", () => {
  assert.equal(satisfiesRange("24.0.0-rc.1", ">=24"), false);
  assert.equal(satisfiesRange("1.2.3-beta", "^1.2.3-beta"), true);
});

test("satisfiesRange: 空 range 视为满足", () => {
  assert.equal(satisfiesRange("1.0.0", ""), true);
  assert.equal(satisfiesRange("1.0.0", null), true);
});
