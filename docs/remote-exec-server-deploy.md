# codex exec-server 公网部署 + 测试指南（Windows x64, wss + token）

本目录交付：
- `codex.exe` —— release 版二进制（既是 server 也是 client，同一个可执行文件）。
- `start-exec-server.bat` —— server 启动脚本（编辑里面的 token 后运行）。
- 本说明。

> 角色说明：`codex.exe exec-server ...` 当 **server**；`codex.exe exec ...` 当 **client**。
> 你把 `codex.exe` 放到公网服务器当 server；我（或你本地）用同一个 exe 当 client 连过去。
> **认证只在 client 侧**（ChatGPT/API key 登录），server 不需要登录，只用 token 做连接鉴权。

---

## 一、在公网服务器上启动 server（你来做）

### 1. 拷贝文件
把 `codex.exe` 和 `start-exec-server.bat` 放到服务器同一目录，例如 `C:\codex\`。

### 2. 设置 token 并启动
编辑 `start-exec-server.bat`，把 `AUTH_TOKEN` 改成一个**长随机串**（例如 32+ 位），然后运行它。

或直接命令行启动（PowerShell）：
```powershell
cd C:\codex
.\codex.exe exec-server --listen "wss://0.0.0.0:8911" --auth-token "你的强随机token"
```

- `wss://` = TLS 加密（server 启动时自动生成自签证书）。
- `0.0.0.0:8911` = 对所有网卡开放，端口 8911（可改）。

### 3. 记下启动日志的两行
启动后 stdout 会打印：
```
wss://0.0.0.0:8911
pinned-sha256: <64位十六进制指纹>
```
**把 `pinned-sha256:` 后面那串指纹发给我**，连同：
- 服务器公网 IP（例如 `203.0.113.5`）
- 端口（默认 8911）
- 你设置的 token

> 证书每次重启都会变 → 指纹也会变。**重启 server 后要把新指纹重新发我。**

### 4. 放行防火墙 / 安全组
确保 **TCP 8911** 入站放行：
- Windows 防火墙：`New-NetFirewallRule -DisplayName "codex-exec-server" -Direction Inbound -Protocol TCP -LocalPort 8911 -Action Allow`
- 云服务器还要在控制台的**安全组**里放行 8911。

---

## 二、客户端连接（我来做 / 你也可本地做）

客户端有两种配置方式，**推荐用配置文件**，省去每次输环境变量、重启换指纹也只改一处。

### 方式 A（推荐）：写 `$CODEX_HOME/environments.toml`

`$CODEX_HOME` 默认是 `~/.codex`（Windows 为 `C:\Users\<你>\.codex`）。新建/编辑 `environments.toml`：
```toml
default = "devbox"          # 让 codex 默认就用这个远程环境

[[environments]]
id  = "devbox"
url = "wss://<你的公网IP>:8911"
auth_token        = "你的强随机token"
tls_pinned_sha256 = "<server 启动打印的指纹>"
```
然后直接跑，无需任何环境变量：
```powershell
.\codex.exe exec --skip-git-repo-check --sandbox danger-full-access "你的任务"
```
> 重启 server 后指纹会变 → 只需改 `environments.toml` 里的 `tls_pinned_sha256` 一行。
> 多台远程机各写一个 `[[environments]]` 块即可，凭据互不影响。

### 方式 B（备用）：环境变量

```powershell
$env:CODEX_EXEC_SERVER_URL               = "wss://<你的公网IP>:8911"
$env:CODEX_EXEC_SERVER_AUTH_TOKEN        = "你的强随机token"
$env:CODEX_EXEC_SERVER_TLS_PINNED_SHA256 = "<你发我的指纹>"

.\codex.exe exec --skip-git-repo-check --sandbox danger-full-access "你的任务"
```
> 注意：一旦存在 `environments.toml`，`CODEX_EXEC_SERVER_URL` 不再被读取（走文件方式）。
> 在 `environments.toml` 中省略 `auth_token` / `tls_pinned_sha256` 时，会回退到对应的环境变量，方便混用。

连接流程（两种方式相同）：
1. client 用指纹锁定校验 server 自签证书（防中间人）。
2. 用 token 通过 `Authorization: Bearer` 鉴权。
3. 之后所有命令/输出/文件经 wss 加密信道下发到 **server 上执行**。

---

## 三、我会怎么测试

拿到你的 IP / 端口 / token / 指纹后，我会：
1. **连通性 + 鉴权**：用正确凭据连上跑一个任务；故意用错 token 验证被 401 拒。
2. **真远程执行**（不可造假）：让模型真跑一条 `Get-Random` 随机数命令，对比 server 端日志确认命令确实在**你的服务器**上执行（而不是我本地兜底）。
3. **指纹锁定**：故意用错误指纹验证 TLS 握手被拒（防中间人有效）。

---

## 四、安全须知（重要）

- **token 要足够随机**，泄露 = 任何人可在你服务器上执行任意命令。
- `danger-full-access` 表示命令在 server 上**无沙箱**执行；公网服务器请用**专用隔离机器**，不要放敏感数据。
- 测试完**及时停掉 server**（关闭窗口 / Ctrl+C），并在防火墙关闭 8911。
- 证书是自签 + 指纹锁定：能防窃听和中间人，但指纹务必通过可信渠道（不要在不安全信道明文传给陌生人）。

---

## 五、常见问题

| 现象 | 原因 / 解决 |
|---|---|
| client 报连接超时 | 防火墙/安全组没放行 8911，或 IP/端口写错 |
| client 报证书指纹不匹配 | server 重启过，指纹变了 → 改 `environments.toml` 的 `tls_pinned_sha256`（或环境变量） |
| client 报 401 | token 不一致 |
| 模型说 "no shell tool available" | client 没连上 server（URL/token/指纹任一错）→ 退化成无执行能力 |
| server 日志 "running WITHOUT authentication" | 你没传 `--auth-token`，公网严禁，请加上 |
