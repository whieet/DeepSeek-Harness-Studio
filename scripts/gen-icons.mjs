// 从 @ant-design/icons-svg 按需生成 ui/icons.gen.js（内联 SVG，按需打包）。
// 用法：node scripts/gen-icons.mjs ；新增图标时在 ICONS 白名单加名字后重跑。
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const ICONS = [
  "FileOutlined",
  "FolderOutlined",
  "FolderOpenOutlined",
  "SearchOutlined",
  "BranchesOutlined",
  "ApartmentOutlined",
  "BarsOutlined",
  "CaretDownOutlined",
  "CaretRightOutlined",
  "CloseOutlined",
  "EllipsisOutlined",
  "ReloadOutlined",
  "HistoryOutlined",
  "RightOutlined",
];

function loadSvg(name) {
  const esPath = join(root, "node_modules/@ant-design/icons-svg/es/asn", name + ".js");
  if (!existsSync(esPath)) throw new Error("icon 不存在: " + name);
  const code = readFileSync(esPath, "utf8");
  // 源码形如: import ...; var svg = {...xml...}; export default svg;
  const m = code.match(/\{[\s\S]*"viewBox"[\s\S]*\}/);
  if (!m) throw new Error("无法解析 icon: " + name);
  return m[0];
}

const out = [
  "// 此文件由 scripts/gen-icons.mjs 自动生成（@ant-design/icons-svg 按需内联）。勿手改。",
  "window.DSH_ICONS = {",
];
for (const name of ICONS) {
  out.push("  " + JSON.stringify(name) + ": " + loadSvg(name) + ",");
}
out.push("};");
const target = join(root, "ui/icons.gen.js");
writeFileSync(target, out.join("\n") + "\n");
console.log("生成", target, "（" + ICONS.length + " 个图标）");
