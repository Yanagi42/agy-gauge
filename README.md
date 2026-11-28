# Gauge

一个**严格只读、极轻量、零联网**的多 Agent 本地 Token 用量、API 费用估算（USD）、官方 `/statusline` 配额桥接与系统负载仪表盘（Windows），原生聚合 **Antigravity（`agy` CLI / App / IDE）+ Claude Code + OpenAI Codex CLI + Gemini CLI**：

- **Rust + Direct2D 桌面悬浮版**（`gauge.exe`）：右下角逐像素抗锯齿毛玻璃胶囊 + Windows 系统托盘常驻 + 即时详情面板（进程内复用增量扫描缓存与 90 秒系统负载历史），默认自动发现本机已安装的全部 Tier-1 纯文本 Agent。
- **Rust 原生 `/statusline` 桥接器**（`agy-statusline.exe` / `agy-statusline.cmd`）：遵循 Antigravity 官方 `statusLine` 命令协议（仅扫描 Antigravity 目录以保持启动与渲染耗时 < 10ms），在终端底部显示紧凑状态栏的同时，把脱敏后的 Antigravity 官方剩余配额与重置倒计时同步给桌面版与 CLI。
- **Python 零依赖命令行版**（`gauge_stats.py` / `gauge-cli.cmd`）：100% 严格只读（单元测试通过 AST 静态约束验证，零文件写操作、零 `sqlite3`、零网络模块），支持多 Agent 聚合与过滤、表格 / 单行摘要 / JSON / CSV 统计、来源分布与预算退出码检查。

## 桌面版特性

- **多 Agent 纯文本聚合 + 轻量归档（`archive.json`）**：
  - 默认自动只读发现并汇总本机已安装的 **Antigravity（CLI / App / IDE）**、**Claude Code**、**OpenAI Codex CLI** 与 **Gemini CLI** 会话日志（可用 `--agy-only` 或 `--agents claude,codex` 过滤）。
  - 仅存在 Antigravity 日志时保持原有简洁风格（仅标注 `[App]` / `[IDE]`）；当检测到非 Antigravity 来源时，在概览最近会话、会话列表、会话详情、调用记录与托盘快捷恢复菜单中展示克制的单色来源前缀 `[AGY]` / `[App]` / `[IDE]` / `[Claude]` / `[Codex]` / `[Gemini]`。
  - 扫描后自动将轻量轮次摘要持久化到程序同目录 `archive.json`（按 Agent 家族隔离键值，避免跨 Agent ID 冲突）；当历史目录被清理时自动回填并标示 `[归档]`，活文件始终优先覆盖、零重复计数（`--no-archive` 可禁用）。
- **多 Agent 终端会话恢复与置顶管理**：
  - 支持在会话详情顶栏、会话右键菜单与托盘「最近会话」子菜单一键恢复终端会话或复制恢复命令，按来源自动分派对应 CLI：
    - Antigravity CLI：`agy --conversation <id>`
    - Claude Code：`claude --resume <id>`
    - OpenAI Codex CLI：`codex resume <id>`
    - Gemini CLI：`gemini --resume <id>`
  - 自动解析 `.exe` / `.cmd` / `.bat`（兼容 Windows npm `.cmd` shim）并校验会话 ID 白名单；子会话自动按同 Agent 家族关联父会话，支持跳转或直接恢复父会话；桌面 App / IDE 与归档会话清晰给出原因提示。
  - **可配置恢复终端**：默认通过 **Windows Terminal (PowerShell)** 打开并加载用户 `$PROFILE` 且退出会话后保留交互 Shell；右键胶囊/托盘菜单「恢复终端」可一键切换内置预设（Windows Terminal PowerShell / 直连 CLI / 独立 PowerShell / WezTerm / Alacritty / Git Bash / cmd / 自定义）、新窗口与保留 Shell 开关，或生成编辑同目录 `terminal.json` 指定 `wt_profile` 与自定义终端命令模板。
  - 支持跨时间范围置顶会话（持久化于程序同目录 `session_meta.json`，不污染统计汇总与导出）。
- **双编辑级主题**：内置 **暖炭黑（Obsidian）** 与 **浅色纸感（Warm Paper）** 两套克制配色（零霓虹高亮），自动调用 `Segoe UI Variable` / `Maple Mono` / `JetBrains Mono`，支持一键切换并同步 Win11 标题栏。
- **Antigravity 官方 `/statusline` 桥接 + 全源预算预警（`budget.json`）**：
  - **边界清晰**：`/statusline` 仅桥接 **Antigravity CLI（`agy`）** 官方剩余配额与重置倒计时；而 **每日 / 每周美元预算** 与悬浮胶囊/顶栏统计**自动汇总全部已发现 Agent**。
  - 接入 `agy-statusline.exe` 后自动热读本地 `statusline_state.json`，清晰标注数据来源（`官方实时` / `官方缓存` / `本地估算`），全程零账号凭证读取、零未公开接口请求；面板内提供一键「复制 `/statusline` 配置」。
  - 在「模型 → 预算 / 桥接」（快捷键 `B`）中可设置每日 / 每周美元预算与预警/临界百分比，保存至程序目录 `budget.json`；触发阈值或官方配额偏低时，胶囊与托盘图标自动切换警示色并按窗口去重弹出系统通知。
- **系统托盘、托盘悬停模式、全屏自动隐藏与开机自启**：
  - 常驻 Windows 系统托盘（左键切换详情面板，右键打开统一菜单与最近 CLI 会话快捷恢复，Explorer 重启自动恢复图标），悬停显示今日全源用量与官方配额摘要。
  - **托盘悬停模式（右键菜单勾选或 `--tray-hover`）**：胶囊不再常占屏幕空间；开启时自动将托盘图标固定展示在任务栏右下角（写入当前用户 `HKCU\Control Panel\NotifyIconSettings` 的 `IsPromoted=1`），切换回常驻悬浮胶囊时自动收回到 `^` 折叠区（`IsPromoted=0`）；鼠标悬停即在任务栏上方浮出实时毛玻璃胶囊。
  - 支持「隐藏悬浮胶囊」仅保留托盘后台监控、「全屏应用自动隐藏胶囊」（`SHQueryUserNotificationState`）以及免提权开机自启（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Gauge`；用户偏好存放在 `HKCU\Software\Gauge` 并自动迁移旧版 `Software\AgyTokenMonitor`）。
- **全局 `Tok | $` 费用视图切换（快捷键 `D`）**：
  - 内置覆盖 Antigravity / Claude（支持 1h 缓存写 `2.0×` 计费）/ OpenAI Codex（`gpt-5.*`、`gpt-5-codex`、`codex-mini`、`gpt-4.1*`）/ Gemini 2.0+ 模型单价，支持在「模型 → 单价表」在线编辑并热写程序目录 `pricing.json`（精确模型名优先于前缀匹配）。
  - 费用模式下，趋势柱图、按天/按周/按月与逐轮剖析全部按美元拆解堆叠（缓存读 / 新输入 / 输出），实时计算**缓存命中已省美元金额**与周环比变化。
- **一键脱敏模式（快捷键 `P` / `--redact`）**：
  - 开启后自动将会话标题/子任务名、项目工作区名、会话 ID、含本机用户名的路径替换为稳定可读的脱敏代号（`会话 #1`、`子任务 #1.1`、`项目 A`、`S-001`、`~/.gemini/antigravity-cli`、`~/.claude`、`~/.config/claude`、`~/.codex`、`~/.gemini/tmp`；自定义目录统一显示 `<已脱敏目录>`），完整保留 Agent 名称、模型名、Token、命中率与估算费用。
  - 脱敏规则同步覆盖面板视图、系统页活跃来源状态、剪贴板复制、**实时搜索框**（脱敏下可搜 Agent 别名、模型名与工具名，但绝不匹配原始敏感词，杜绝侧信道）以及 **CSV / JSON 导出文件**。
- **右下角轻量胶囊**：
  - `UpdateLayeredWindow` 逐像素圆角与柔和阴影，不占任务栏/Alt-Tab、不抢焦点；支持自由拖拽跨屏记忆位置与右键复位。
  - 右键「胶囊显示内容」支持 8 项自由组合：**今日总 Token / 今日估算费用 ($) / 官方配额 (Q · AGY) / 今日非缓存增量 Δ / 今日缓存命中率 % / 活跃会话上下文 Ctx / CPU 波形 / 内存环**。
- **扁平 6 视图 + 热力图 + 交互搜索与导出**：
  1. **概览**：智能趋势图（今日分时 / 多日近 7–14 天）+ **「官方额度 | 本周同比」** + **「模型 | 项目 | 工具 | 来源」**四维切换（点击任意柱体 / 模型 / 项目 / Agent 来源直接钻取会话）+ 全宽最近活跃会话表。
  2. **按天**：独立于顶栏时间范围、始终基于全量历史展示（顶栏显示「全量历史」，不改写原范围偏好）；内置 **「按天柱图 | 活跃热力图」**切换——近 30 天柱图、20 周 × 7 天陶土色阶日历热力图与可滚动全量每日明细表，点击任意日期格或行直接钻取该日会话（若超出当前范围自动切至「全部」）。
  3. **按周**：独立于顶栏时间范围、基于全量完整自然周期聚合，避免窄范围截断首尾周/月导致环比失真；支持 **「按周 (ISO) | 按月」**切换（含周环比、活跃天、日均与估算费用），点击任意周直接钻取整周会话。
  4. **模型**：内置 **「缓存对比 | 单价表 ($) | 预算 / 桥接」**三合一子视图，支持横向对比稳定命中率与**输出速度（tok/s）**、在线修改单价表、配置全源预算阈值与查看 Antigravity 官方配额池。
  5. **会话**：三段式视觉分组（`轮数` — **`总Token` / `新输入` / `输出`** — `命中率` / `费用`）；自动识别并按同 Agent 家族归组子代理（**`子会话:归组` / `独立`**）；内置**实时关键词搜索框（按 `/` 聚焦，支持搜标题、项目、模型、工具名与来源别名如 `claude` / `codex` / `gemini-cli` / `agy`）**；底部剖析卡集成一键**继续 / 复制命令 / 打开目录 / 置顶**，展示有效活跃时长、Thinking 轮次占比、高频工具与逐轮柱图。
  6. **调用记录（按 `L` 或点击顶栏「调用」）**：按时间倒序平铺每一次带 Token 计数的模型调用，三段式展示 **总耗时 / tok/s** — **`总Token` / `新输入` / `输出`** — **命中率 / 费用**；共用搜索框与快捷键 `↑`/`↓` 切换，底部明细卡展示完整 Token、费用、工具列表与思考量（Codex / Gemini CLI 展示 `思考 X tok`，Antigravity / Claude Code 展示 `思考 X 字`）。
     - **总耗时与速度的优雅降级**：Antigravity transcript 通过请求与派生工具步骤的秒级时间戳估算端到端总耗时与输出速度（含首字延迟与思考；耗时不足 3 秒或输出不足 64 Token 不计速度）；纯文本回复及不记录总耗时的外部 Agent 日志（Claude Code / Codex / Gemini CLI）均优雅显示 `-`，剪贴板明细注明「当前日志无总耗时记录」。不拦截 API 流量，因此不提供首字延迟（TTFT）。
  7. **系统监控（点击 Hero 右侧 CPU/内存圆环或按 `6`）**：90 秒 CPU / 物理内存渐变面积折线图（悬停回看）+ 双圆环硬件表 + 动态展示全部活跃只读来源目录、增量缓存命中与归档回填状态。
  8. **自定义日期与导出**：时间范围支持 **`今日` / `7天` / `30天` / `全部` / `自定义`**（`YYYY-MM-DD`）；顶栏 **`导出`** 支持原生保存框导出 **完整统计快照（JSON，含 `by_agent`）**、**当前会话列表（UTF-8 BOM CSV）**、**当前调用记录（CSV）**、**每日汇总表（CSV）** 或复制 `/statusline` 配置片段。
- **快捷键**：`1`–`5` 切换标签、`L` 调用记录、`6` 系统监控、`B` 预算与桥接、`/` 聚焦搜索（会话 / 调用页）、`D` 切换 `Tok`/`$`、`P` 脱敏模式、`T` 总计/仅增量、`Enter` 恢复选中会话、`Ctrl+Shift+C` 复制恢复命令、`O` 打开工作区目录、`S` 置顶/取消置顶、`↑`/`↓` 切换会话或调用、`Backspace` 清除筛选、`R`/`F5` 刷新、`Ctrl+C` 复制摘要、`Esc` 退出输入框或隐藏面板。

## 官方 `/statusline` 桥接配置（可选，仅 Antigravity CLI）

`/statusline` 桥接**仅用于 Antigravity CLI（`agy`）官方配额池同步**（每日/每周美元预算仍自动聚合全部已发现 Agent）。在桌面版点击「复制 `/statusline` 配置」（或运行 `.\agy-statusline.cmd --print-config`），将生成的 `statusLine` 片段合并到 `~/.gemini/antigravity-cli/settings.json`（本工具遵守只读底线，绝不自动修改 `settings.json`）：

```json
{
  "statusLine": {
    "type": "command",
    "command": "\"C:\\path\\to\\agy-statusline.exe\"",
    "stack_with_default": false
  }
}
```

每次 `agy` 刷新底部状态栏时，会通过标准输入向 `agy-statusline.exe` 传入官方 JSON；`agy-statusline.exe` 在 10ms 内输出精简终端状态栏，同时将脱敏后的配额快照原子写入同目录 `statusline_state.json`（自动剥离工作区路径、会话 ID 与对话内容）。

## 自定义单价、预算与会话元数据（仅位于程序同目录）

以下 JSON 文件均仅读写于 `gauge.exe` 同目录，**绝不写入 `~/.gemini`、`~/.claude` 或 `~/.codex`**：

- `pricing.json`：自定义模型单价热覆盖（可在「模型 → 单价表」在线编辑，单位 USD / 1M tokens）：
  ```json
  {
    "models": {
      "gemini-4-neon": { "input": 3.0, "cache_read": 0.15, "output": 15.0 },
      "claude-opus-5-5": { "input": 4.0, "cache_read": 0.20, "cache_write": 5.0, "output": 20.0 }
    }
  }
  ```
- `budget.json`：全源每日 / 每周美元预算与预警/临界百分比（可在「模型 → 预算 / 桥接」在线编辑）。
- `statusline_state.json`：由 `agy-statusline.exe` 写入的 Antigravity 官方配额脱敏缓存。
- `archive.json`：本地历史轮次轻量归档（可用 `--no-archive` 禁用）。
- `session_meta.json`：用户置顶会话标记（与 Agent 原始日志目录完全隔离）。
- `terminal.json`（可选）：恢复会话终端高级配置（仅在右键「恢复终端 → 打开 / 生成 terminal.json...」或手动创建时使用；支持指定 Windows Terminal Profile `wt_profile` 与 `custom` 模板占位符 `{dir}`、`{exe}`、`{arg}`、`{cid}`、`{title}`、`{cmd}`、`{pwsh}`）：
  ```json
  {
    "preset": "wt_pwsh",
    "wt_profile": "PowerShell",
    "wt_new_window": false,
    "keep_shell_open": true,
    "custom": {
      "exe": "",
      "args": ["--working-directory", "{dir}", "-e", "{pwsh}", "-NoLogo", "-NoExit", "-Command", "{cmd}"]
    }
  }
  ```

## 安全与只读保证（路径白名单与红线）

对全部本地 Agent 数据目录实行**严格只读 + 规范路径边界校验 + 拒绝符号链接**，仅打开下列纯文本白名单文件：

- **Antigravity（CLI / App / IDE）**：`~/.gemini/antigravity{,-cli,-ide}`
  - 仅读：`brain/<id>/.system_generated/logs/transcript.jsonl`（缺失回退 `transcript_full.jsonl`）、`annotations/<id>.pbtxt`、`cache/conversation_metadata.json`
  - 红线：**绝不打开** `conversations/*.db` 或任何 SQLite 文件，避免与活跃会话产生 WAL/SHM 锁冲突
- **Claude Code**：`$CLAUDE_CONFIG_DIR`（逗号分隔）或 `~/.claude` / `~/.config/claude`
  - 仅读：`projects/<proj>/<uuid>.jsonl`、`.../subagents/agent-*.jsonl`、可选 `sessions-index.json` 与 `agent-*.meta.json`
  - 红线：**绝不读取** `settings*.json`、`.credentials.json`、`auth*`、`history.jsonl`
- **OpenAI Codex CLI**：`$CODEX_HOME` 或 `~/.codex`
  - 仅读：`sessions/YYYY/MM/DD/rollout-*.jsonl`、`archived_sessions/rollout-*.jsonl`、可选 `session_index.jsonl`
  - 红线：**绝不打开** `*.sqlite*`（含 WAL/SHM 活跃数据库）、`auth.json`、`config.toml`、`history.jsonl`
- **Gemini CLI**：`$GEMINI_CLI_HOME` 或 `~/.gemini`
  - 仅读：`tmp/<proj>/chats/session-*.{json,jsonl}`、可选 `.project_root` 与 `projects.json`
  - 红线：**绝不读取** `oauth_creds.json`、`google_accounts.json`、`jetski-standalone-oauth-token`、`settings.json`
- **零联网、零凭证、零写入**：全程无任何网络请求与账号令牌访问；Python CLI 通过单元测试中的 AST 静态检查强制保障零文件写操作、零 `sqlite3` 导入与零网络模块导入。

## 快速开始

### 从源码编译（Rust GNU 工具链 + MinGW `windres`）

```powershell
cd rust
cargo test --all-targets
cargo build --release
copy target\release\gauge.exe ..\gauge.exe
copy target\release\agy-statusline.exe ..\agy-statusline.exe
```

### 运行命令

```powershell
# 启动桌面悬浮胶囊与系统托盘（默认自动发现 Antigravity + Claude Code + Codex CLI + Gemini CLI）
.\gauge.exe

# 过滤来源：仅监控 Antigravity / 仅监控指定 Agent 组合 / 兼容指定单个数据目录
.\gauge.exe --agy-only
.\gauge.exe --agents claude,codex
.\gauge.exe --data-dir "$env:USERPROFILE\.gemini\antigravity-cli"

# 常用显示参数：托盘悬停模式 / 极简角标 / 浅色纸感 / 费用视图 / 脱敏模式 / 禁用本地归档 / 指定副屏 / 指定单价表
.\gauge.exe --tray-hover
.\gauge.exe --mini --light --cost --redact
.\gauge.exe --no-archive --monitor 1 --pricing .\pricing.json

# 生成 Antigravity CLI 官方 /statusline 配置片段
.\agy-statusline.cmd --print-config

# Python 零依赖命令行版（严格只读，默认自动发现全部 Tier-1 Agent；也可直接用 .\gauge-cli.cmd）
python gauge_stats.py --today --human
python gauge_stats.py --view sources --human
python gauge_stats.py --agy-only --today --human
python gauge_stats.py --agent claude,codex --days 7 --human
python gauge_stats.py --compact
python gauge_stats.py --view tools --days 7
python gauge_stats.py --view weekly --sort cost --human
python gauge_stats.py --view all --redact
python gauge_stats.py --format json --redact
# 脚本化全源预算检查（退出码：0 正常 / 10 预警 / 20 临界）
python gauge_stats.py --budget-daily 30 --budget-weekly 150 --check-budget
```
