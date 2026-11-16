// A small HTTP server on 127.0.0.1 for browser redirects (login and the GitHub authorization).
// node:http, so it runs the same in Bun and in Node.js.
import { createServer } from "node:http";

export interface LoopbackReply {
  status: number;
  headers?: Record<string, string>;
  body?: string;
}

export interface LoopbackServer {
  port: number;
  /**
   * Stop listening. With `force`, open connections are closed too, so the port is free at once;
   * without it, requests in flight finish first.
   */
  stop(force?: boolean): Promise<void>;
}

/** Listen on 127.0.0.1:port (0 picks a free port) and answer each request with handle's reply. */
export function serveLoopback(port: number, handle: (method: string, url: URL) => LoopbackReply): Promise<LoopbackServer> {
  return new Promise((resolve, reject) => {
    const server = createServer((req, res) => {
      let reply: LoopbackReply;
      try {
        reply = handle(req.method ?? "GET", new URL(req.url ?? "/", "http://127.0.0.1"));
      } catch {
        reply = { status: 500, body: "internal error" };
      }
      res.writeHead(reply.status, reply.headers);
      res.end(reply.body ?? "");
    });
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => {
      server.off("error", reject);
      const addr = server.address();
      resolve({
        port: typeof addr === "object" && addr ? addr.port : port,
        stop: (force) =>
          new Promise<void>((done) => {
            server.close(() => done());
            if (force) server.closeAllConnections();
            else server.closeIdleConnections();
          }),
      });
    });
  });
}
