# e-agent Windows 安装脚本
# 用法（PowerShell）：
#   powershell -ExecutionPolicy Bypass -File scripts\install-release.ps1
#
# 做三件事：
#   1. cargo install 编译并安装 e-agent（release 版，含 greptime + sqlite/turso）
#   2. 可选：在 ~/.cargo/bin 下创建 e.cmd 别名（让 `e web -p 8766` 可用）
#   3. 提示后续配置（config.toml 的 [session] 段 + 防火墙放行）
$ErrorActionPreference = "Stop"

Write-Host "== e-agent Windows 安装 ==" -ForegroundColor Cyan

# 1. 编译安装（默认 features = greptime + sqlite/turso；--locked 用锁定版本）
Write-Host "[1/3] cargo install --path . --locked ..." -ForegroundColor Yellow
cargo install --path . --locked
if ($LASTEXITCODE -ne 0) { throw "cargo install 失败" }

# 2. 创建 e.cmd 别名（在 cargo bin 目录，若已存在则跳过）
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
$eCmd = Join-Path $cargoBin "e.cmd"
if (-not (Test-Path $eCmd)) {
    $eAgent = Join-Path $cargoBin "e-agent.exe"
    if (Test-Path $eAgent) {
        "@`"$eAgent`" %*" | Set-Content -Path $eCmd -Encoding ASCII
        Write-Host "[2/3] 已创建 $eCmd（e-agent 别名，`e web -p 8766` 可用）" -ForegroundColor Green
    } else {
        Write-Host "[2/3] 未找到 $eAgent，跳过别名" -ForegroundColor Yellow
    }
} else {
    Write-Host "[2/3] $eCmd 已存在，跳过" -ForegroundColor Yellow
}

# 3. 配置提示
Write-Host "[3/3] 后续配置提示：" -ForegroundColor Yellow
Write-Host "  - 配置文件：%USERPROFILE%\.config\e-agent\config.toml"
Write-Host "  - 推荐 [session] 段（本地内嵌 turso/SQLite，无需外部服务）："
Write-Host "      [session]"
Write-Host "      backend = \"sqlite\""
Write-Host "      path = \"%USERPROFILE%\\.local\\share\\e-agent\\sessions.db\""
Write-Host "  - 局域网访问（供 Linux 聚合）：e web --host 0.0.0.0 --port 8766"
Write-Host "  - 防火墙放行 8766 端口（首次启动时 Windows 会弹窗，或手动："
Write-Host "      netsh advfirewall firewall add rule name=\"e-agent\" dir=in action=allow protocol=TCP localport=8766)"
Write-Host ""
Write-Host "安装完成！运行 e web --host 0.0.0.0 --port 8766 启动。" -ForegroundColor Green
