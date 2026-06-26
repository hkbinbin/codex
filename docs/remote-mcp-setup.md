# 让 MCP 在远程 exec-server 上运行（Skills 保持本地）

> 适用场景：你在**一台远端机器**上放了一些 exe / MCP server，希望 codex 调用的 MCP 工具在那台机器上执行；而 Skills（SKILL.md）只用来拼 prompt，从本地读即可。
>
> 结论：**纯配置即可，无需改代码。** 远程 MCP 的机制（`ExecutorStdioServerLauncher`）已经存在，配合阶段一/三做的 token + wss 加密信道直接可用。

---

## 一、原理（已核实代码）

| 能力 | 在哪台机器 | 依据 |
|---|---|---|
| **Skills**（拼 prompt） | **始终本地** | `core-skills/src/loader.rs` 中 User/System/Admin scope 硬编码 `LOCAL_FS`；SKILL.md 仅注入 prompt |
| **MCP stdio server** | 由 `environment_id` 决定 | `codex-mcp/src/rmcp_client.rs:699`：非 `local` 时用 `ExecutorStdioServerLauncher` 把进程下发远端 |
| **MCP 协议帧**（initialize/tools/list/tools/call） | 始终在 client | rmcp 在 client 进程内处理，仅 stdin/stdout 字节经 exec-server 透传 |

**解耦点**：MCP 走自己的 `environment_id` 选环境，**与 turn 是否远程无关**。这正好满足「MCP 远程、Skills/会话本地」。

---

## 二、配置步骤

### 1. 远端机器：启动 exec-server（wss + token）
```bash
codex exec-server --listen wss://0.0.0.0:8999 --auth-token YOUR_TOKEN
```
启动日志会打印两行，记录下来：
```
wss://0.0.0.0:8999
pinned-sha256: <64位十六进制指纹>
```

### 2. 本地 client：设置全局凭据环境变量
```bash
export CODEX_EXEC_SERVER_AUTH_TOKEN=YOUR_TOKEN
export CODEX_EXEC_SERVER_TLS_PINNED_SHA256=<上一步的指纹>
```
> Windows PowerShell：`$env:CODEX_EXEC_SERVER_AUTH_TOKEN="YOUR_TOKEN"` 等。

### 3. 定义远程环境 `$CODEX_HOME/environments.toml`
```toml
[[environments]]
id  = "devbox"
url = "wss://<远端host>:8999"
# connect_timeout_sec / initialize_timeout_sec 可选
```
> token 和 TLS 指纹由步骤 2 的全局变量提供，单台机器无需在 TOML 里区分。
> 注意：一旦存在 `environments.toml`，`CODEX_EXEC_SERVER_URL` 环境变量将不再被读取。

### 4. 让 MCP 指向远程环境 `$CODEX_HOME/config.toml`
```toml
[mcp_servers.demo]
command        = "/remote/path/to/your-mcp-server"   # ⚠️ 必须是远端机器上的路径
args           = ["--flag"]
environment_id = "devbox"
cwd            = "/remote/work/dir"                   # ⚠️ 远程 stdio MCP 必填，见下
```

---

## 三、三个必踩的坑（已从代码确认）

### 坑 1：远程 stdio MCP 必须显式提供 `cwd`
`rmcp-client/src/stdio_server_launcher.rs:479-483`：远程 launcher 在 `cwd` 缺省时直接报错
`"executor stdio server requires an explicit cwd"`。
→ **远程 MCP server 的 `[mcp_servers.x]` 必须写 `cwd`，且是远端路径。** 本地 MCP 则可省略。

### 坑 2：`command` / `cwd` / `args` 都是「远端」语义
程序在远端 spawn，所有路径相对远端机器解析。client 本地有没有这个文件**无关紧要**。

### 坑 3：要读远端环境变量，用 `source = "remote"`
默认 codex 会把 client 的环境变量拷给 MCP。若想让某个变量从**远端**环境读取（例如远端机上的 API key）：
```toml
[mcp_servers.demo]
command        = "/remote/path/your-mcp-server"
environment_id = "devbox"
cwd            = "/remote/work/dir"
env_vars       = [
  "PATH",                                  # 普通：从本地拷贝
  { name = "REMOTE_API_KEY", source = "remote" },  # 从远端环境读取
]
```
依据：`config/src/mcp_types.rs:88` + `stdio_server_launcher.rs:560` 的 `remote_env_policy`——`source="remote"` 时 launcher 用 `inherit=All` 让远端进程能读到该变量，再用 `include_only` 收窄。

> 另：远程 stdio 要求 command/args/env 全部是合法 UTF-8（`stdio_server_launcher.rs:554`），非 Unicode 会被拒。

---

## 四、验证清单

1. **启动 codex**，确认目标 MCP server 在**远端** exec-server 上拉起（远端进程列表可见，本地无该进程）。
2. **`tools/list`**：能列出该 MCP 暴露的工具。
3. **`tools/call`**：调用一个会触发远端 exe 的工具，确认 exe 在远端执行、结果正确回传。
4. 若配了 `source="remote"` 的 `env_vars`，确认变量在远端取到正确值。
5. 反向验证：故意把 token 改错 → 连接应被 401 拒绝（验证加密信道鉴权生效）。

---

## 五、Skills 维持本地（无需任何操作）

- User（`$CODEX_HOME/skills`、`$HOME/.agents/skills`）、System、Admin scope 的 skill **始终从 client 本地读**，SKILL.md 文本注入 prompt。这是现状，符合「只拼 prompt」的诉求。
- ⚠️ 唯一注意：若某 skill 带 `scripts/` 并指示模型用 shell 工具去执行**本地脚本路径**，而当前 turn 跑在远程环境，远端没有该路径会失败。**纯 prompt 型 skill 不受影响。** 需要远端可执行的脚本，应作为远端 exe + MCP 工具暴露，而非塞进 skill 的 scripts。

---

## 六、本方案不改动任何代码
- MCP 远程 launcher、environments.toml schema、skills loader 均已具备所需能力。
- 仅当验证中发现远程 MCP 链路存在 bug 时，才需回到代码层修复。
