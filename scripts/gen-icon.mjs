// 生成 1024x1024 应用图标 PNG（零依赖，手写 PNG 编码）：深色圆角底 + 蓝紫渐变方块 + 白色命令符。
import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";

const SIZE = 1024;
const pixels = Buffer.alloc(SIZE * SIZE * 4, 0);

function setPixel(x, y, r, g, b, a) {
  const offset = (y * SIZE + x) * 4;
  pixels[offset] = r;
  pixels[offset + 1] = g;
  pixels[offset + 2] = b;
  pixels[offset + 3] = a;
}

function roundedSquareDistance(x, y, x0, y0, x1, y1, radius) {
  const cx = Math.min(Math.max(x, x0 + radius), x1 - radius);
  const cy = Math.min(Math.max(y, y0 + radius), y1 - radius);
  const inside = x >= x0 + radius && x <= x1 - radius && y >= y0 + radius && y <= y1 - radius;
  if (inside) return -1;
  const dx = Math.max(x0 - x, 0, x - x1);
  const dy = Math.max(y0 - y, 0, y - y1);
  const outsideCorner = (x < x0 + radius || x > x1 - radius) && (y < y0 + radius || y > y1 - radius);
  if (outsideCorner) {
    return Math.hypot(x - cx, y - cy) - radius;
  }
  return Math.max(dx, dy);
}

function segmentDistance(px, py, ax, ay, bx, by) {
  const abx = bx - ax;
  const aby = by - ay;
  const apx = px - ax;
  const apy = py - ay;
  const lengthSq = abx * abx + aby * aby;
  let t = lengthSq === 0 ? 0 : (apx * abx + apy * aby) / lengthSq;
  t = Math.min(Math.max(t, 0), 1);
  return Math.hypot(px - (ax + abx * t), py - (ay + aby * t));
}

for (let y = 0; y < SIZE; y++) {
  for (let x = 0; x < SIZE; x++) {
    const distance = roundedSquareDistance(x, y, 64, 64, 960, 960, 180);
    if (distance <= 0) {
      const t = (y - 64) / (960 - 64);
      const r = Math.round(46 + (122 - 46) * t);
      const g = Math.round(124 + (92 - 124) * t);
      const b = Math.round(246 + (246 - 246) * t);
      // 命令符 ">"：两段粗线
      const d1 = segmentDistance(x, y, 360, 330, 610, 512);
      const d2 = segmentDistance(x, y, 610, 512, 360, 694);
      const glyph = Math.min(d1, d2) < 46;
      if (glyph) {
        setPixel(x, y, 255, 255, 255, 255);
      } else {
        setPixel(x, y, r, g, b, 255);
      }
    }
  }
}

// ── PNG 编码 ──────────────────────────────────────────────────────────────────
const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c;
  }
  return table;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (const byte of buf) {
    c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8;  // bit depth
ihdr[9] = 6;  // RGBA
const raw = Buffer.alloc(SIZE * (SIZE * 4 + 1));
for (let y = 0; y < SIZE; y++) {
  raw[y * (SIZE * 4 + 1)] = 0; // filter: none
  pixels.copy(raw, y * (SIZE * 4 + 1) + 1, y * SIZE * 4, (y + 1) * SIZE * 4);
}
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);
writeFileSync(new URL("./icon-1024.png", import.meta.url), png);
console.log("icon written: scripts/icon-1024.png (" + png.length + " bytes)");
