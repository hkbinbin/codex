@echo off
REM ============================================================
REM  codex exec-server 公网启动脚本 (Windows x64, wss + token)
REM
REM  用法：双击运行，或在 PowerShell/cmd 里执行。
REM  退出：关闭窗口或 Ctrl+C。
REM ============================================================

REM ---- 1) 监听地址：0.0.0.0 表示对公网所有网卡开放，端口可改 ----
set LISTEN=wss://0.0.0.0:8911

REM ---- 2) 鉴权 token：务必改成你自己的强随机串！----
REM     客户端必须用同一个 token 才能连上。
set AUTH_TOKEN=CHANGE_ME_to_a_long_random_secret

REM ---- 3) 启动 ----
echo Starting codex exec-server on %LISTEN% ...
echo.
echo === 记下下面两行，客户端要用 ===
echo   wss URL : %LISTEN:0.0.0.0=^<你的公网IP^>%
echo   (启动后日志会打印真实监听地址和 pinned-sha256 证书指纹)
echo.

codex.exe exec-server --listen "%LISTEN%" --auth-token "%AUTH_TOKEN%"
