# agent-send-mcp

`agent-send-mcp` is an optional, transport-thin MCP-style adapter. It reads
newline-delimited JSON-RPC 2.0 messages from stdin and writes one response per
line to stdout. It does not access files or implement transfers: every tool
call becomes the daemon's authenticated `POST /v1/agent` request.

Set the scoped bearer token before starting it:

```sh
AGENT_SEND_AGENT_TOKEN=agent_... \
AGENT_SEND_DAEMON_ADDR=127.0.0.1:41641 \
cargo run -p agent-send-mcp
```

`AGENT_SEND_DAEMON_ADDR` defaults to `127.0.0.1:41641`. The token is sent only
in the loopback HTTP `Authorization` header and is never included in responses
or diagnostics. The daemon validates the token's peer and named-folder scope.
`transfers.send` accepts folder IDs and relative source paths, never a
 destination filesystem path. Exposed tools are limited to `peers.list`,
`folders.list`, `transfers.send`, `transfers.status`, and `transfers.cancel`.
