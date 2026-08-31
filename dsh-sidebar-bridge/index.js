// dsh-sidebar-bridge — DeepSeek Harness 桌面版侧边栏数据桥（只读）。
// 独立监听 127.0.0.1:<OS 分配端口>；GET /state 返回会话与工作区摘要，
// 端口写入 ~/.dsh/sidebar-bridge.port 供桌面壳发现。仅回环、仅只读。
import { createServer } from 'node:http'
import { writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

export const name = 'dsh-sidebar-bridge'

export const inject = ['sessionQuery', 'workspaceRegistry']

export const Config = null

export function apply(ctx) {
  const server = createServer(async (req, res) => {
    if (req.url !== '/state') {
      res.writeHead(404).end()
      return
    }
    try {
      const records = await ctx.sessionQuery.listSessions()
      const sessions = records.map((r) => ({
        id: r.header.id,
        cwd: r.header.cwd ?? null,
        createdAt: r.header.createdAt,
        live: r.live,
        persisted: r.persisted,
      }))
      const workspaces = ctx.workspaceRegistry.list().map((w) => ({
        id: w.id,
        path: w.path,
        title: w.title,
        sessionIds: [...w.sessionIds],
      }))
      const newest = records.find((r) => typeof r.header.cwd === 'string')
      res.writeHead(200, { 'content-type': 'application/json; charset=utf-8' })
      res.end(JSON.stringify({
        ok: true,
        currentCwd: newest ? newest.header.cwd : process.cwd(),
        sessions,
        workspaces,
      }))
    } catch (error) {
      res.writeHead(503, { 'content-type': 'application/json; charset=utf-8' })
      res.end(JSON.stringify({ ok: false, error: String((error && error.message) || error) }))
    }
  })
  server.listen(0, '127.0.0.1', () => {
    const port = server.address().port
    try {
      writeFileSync(join(homedir(), '.dsh', 'sidebar-bridge.port'), String(port))
    } catch {}
    console.log('[dsh-sidebar-bridge] listening on 127.0.0.1:' + port)
  })
  ctx.effect(() => () => {
    server.closeAllConnections?.()
    server.close()
  })
}
