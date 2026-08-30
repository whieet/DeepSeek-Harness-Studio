// 测试用假内核：模拟 dsh web 子进程协议（就绪 URL 行 / HTTP 200 / 崩溃 / 立即失败）。
import { createServer } from "node:http";

const failNow = process.argv.includes("--fail");
const crash = process.argv.includes("--crash");
const port = Number(process.argv[process.argv.indexOf("--port") + 1] ?? 0);

if (failNow) {
  console.error("fake-sidecar: 模拟启动即失败");
  process.exit(1);
}

const server = createServer((req, res) => {
  res.writeHead(200, { "content-type": "text/html" });
  res.end("<!doctype html><html><body>fake kernel</body></html>");
});

server.listen(port, "127.0.0.1", () => {
  const bound = server.address().port;
  console.log("dsh web: http://127.0.0.1:" + bound);
  if (crash) setTimeout(() => { console.error("fake-sidecar: 模拟内核崩溃"); process.exit(1); }, 800);
});

process.on("SIGTERM", () => process.exit(0));
