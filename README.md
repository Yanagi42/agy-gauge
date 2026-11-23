# agy-gauge

一个**严格只读、极轻量、零联网**的 Antigravity（`agy` CLI / App / IDE）本地 Token 用量、API 费用估算（USD）、官方 `/statusline` 配额桥接与系统负载仪表盘（Windows）：

- **Rust + Direct2D 桌面悬浮版**（`agy-token-monitor.exe`）：右下角逐像素抗锯齿毛玻璃胶囊 + Windows 系统托盘常驻 + 即时详情面板（进程内复用增量扫描缓存与 90 秒系统负载历史）。
- **Rust 原生 `/statusline` 桥接器**（`agy-statusline.exe` / `agy-statusline.cmd`）：遵循 Antigravity 官方 `statusLine` 命令协议（启动与渲染耗时 < 10ms），在终端底部显示紧凑状态栏的同时，把脱敏后的官方剩余配额与重置倒计时同步给桌面版与 CLI。
- **Python 零依赖命令行版**（`agy_token_stats.py`）：100% 严格只读（单元测试通过 AST 静态约束验证），支持表格 / 单行摘要 / JSON / CSV 统计、5h 账单块、工具分布与预算退出码检查。

## 桌面版特性

- **双编辑级主题**：内置 **暖炭黑（Obsidian）** 与 **浅色纸感（Warm Paper）** 两套克制配色，自动调用 `Segoe UI Variable` / `Maple Mono` / `JetBrains Mono`，支持一键切换并同步 Win11 标题栏。
- **官方 `/statusline` 合规配额桥接 + 5h 滚动账单块**：
  - 概览页内置「**额度 / 5h | 本周同比**」卡片，实时显示当前 5 小时滚动账单块（消耗、烧钱/烧 Token 速率、窗口结束预测与重置倒计时）及 Pace 进度条（对比用量进度与时间进度）。
  - 接入 `agy-statusline.exe` 后自动热读本地 `statusline_state.json`，清晰标注数据来源（`官方实时` / `官方缓存` / `本地估算`），全程零账号凭证读取、零未公开接口请求。
  - 面板内提供一键「复制 `/statusline` 配置」，粘贴进 `agy` 的 `settings.json` 即可启用。
- **预算阈值与托盘气球预警（`budget.json`）**：
  - 在「模型 → 预算 / 桥接」（快捷键 `B`）中可设置 **5h / 每日 / 每周美元预算** 与 **预警 / 临界百分比**，保存至程序目录 `budget.json`。
  - 达到预警/临界阈值或官方剩余配额过低时，悬浮胶囊数值与状态点自动切换警示色，并弹出按窗口去重的 Windows 托盘气球提醒（右键菜单可开关）。
- **系统托盘、托盘悬停模式、全屏自动隐藏与开机自启**：
  - 常驻 Windows 系统托盘（左键切换详情面板，右键打开统一菜单，Explorer 重启自动恢复图标），默认悬停显示今日用量、5h 窗口与官方配额摘要。
  - **托盘悬停模式（右键菜单勾选或 `--tray-hover`）**：胶囊不再常占屏幕空间；开启时自动将托盘图标固定展示在任务栏右下角（写入当前用户 `HKCU\Control Panel\NotifyIconSettings` 的 `IsPromoted=1`，不收进 `^` 折叠区，像输入法图标一样常驻状态栏）；鼠标悬停即在任务栏正上方贴边浮出实时毛玻璃胶囊，点击图标或胶囊即时打开/关闭详情面板。
  - 支持「隐藏悬浮胶囊」仅保留托盘后台监控、「全屏应用自动隐藏胶囊」（通过 `SHQueryUserNotificationState` 检测游戏/演示全屏）以及免提权开机自启（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run\AgyGauge`）。
- **Antigravity 多端聚合 + 本地轻量归档（`archive.json`）**：
  - 自动只读聚合本机存在的 `~/.gemini/antigravity-cli`（CLI）、`~/.gemini/antigravity`（App）与 `~/.gemini/antigravity-ide`（IDE）会话日志，在会话列表标注来源标签 `[App]` / `[IDE]`。
  - 扫描后自动将轻量轮次摘要持久化到程序同目录 `archive.json`；当 `~/.gemini` 清理历史目录时，已归档会话自动回填并标示 `[归档]`，活文件始终优先覆盖、零重复计数（启动参数 `--no-archive` 可禁用）。
- **全局 `Tok | $` 费用视图切换（快捷键 `D`）**：
  - 顶栏一键在 **Token 用量** 与 **估算 API 费用（USD）** 之间切换；Token 模式下 Hero 与明细表同样常驻显示 `$` 估算金额。
  - 费用模式下，趋势柱图、按天/按周/按月与逐轮剖析全部按美元拆解堆叠（缓存读 / 新输入 / 输出），实时计算**缓存命中已省美元金额**与周环比变化；支持在线编辑单价表并热写程序目录 `pricing.json`（精确模型名优先于前缀匹配）。
- **一键脱敏模式（快捷键 `P` / `--redact`）**：
  - 开启后自动将会话标题/子任务名、项目工作区名、会话 ID、含本机用户名的路径替换为稳定可读的脱敏代号（`会话 #1`、`子任务 #1.1`、`项目 A`、`S-001`、`~/.gemini/antigravity-cli`），完整保留模型名、Token、命中率与估算费用。
  - 脱敏规则同步覆盖面板视图、剪贴板复制、**会话关键词搜索**（脱敏下不匹配原始敏感词，杜绝侧信道）以及 **CSV / JSON 导出文件**。
- **右下角轻量胶囊**：
  - `UpdateLayeredWindow` 逐像素圆角与柔和阴影，不占任务栏/Alt-Tab、不抢焦点；支持自由拖拽跨屏记忆位置与右键复位。
  - 右键「胶囊显示内容」支持 9 项自由组合：**今日总 Token / 今日估算费用 ($) / 5h 窗口 / 官方配额 / 非缓存增量 Δ / 缓存命中率 % / 活跃上下文 Ctx / CPU 波形 / 内存环**。
- **扁平 5 视图 + 热力图 + 交互搜索与导出**：
  1. **概览**：智能趋势图（今日分时 / 多日近 7–14 天）+ **「额度 / 5h | 本周同比」** + **「模型 | 项目 | 工具」**切换（点击任意柱体/模型/项目直接钻取）+ 全宽最近活跃会话表。
  2. **按天**：内置 **「按天柱图 | 活跃热力图」**切换——20 周 × 7 天陶土色阶日历热力图（悬停查看当日详情，点击任意日期格直接钻取该日会话）与可滚动每日明细表。
  3. **按周**：ISO 自然周（周一到周日，含周环比、活跃天、日均与估算费用）与 **「按周 (ISO) | 按月」**切换。
  4. **模型**：内置 **「缓存对比 | 单价表 ($) | 预算 / 桥接」**三合一子视图，支持横向对比稳定命中率、在线修改单价表、配置预算阈值与查看近期 5h 账单块。
  5. **会话**：自动识别子代理并支持 **`子会话:归组` / `独立`**；内置**实时关键词搜索框（按 `/` 聚焦，支持搜标题、项目、模型、工具名与来源）**；底部剖析卡展示**有效活跃时长、Thinking 轮次占比、高频工具 Top 2** 与逐轮上下文/费用柱图（悬停显示该轮调用的工具列表与 Thinking 字符数）。
  6. **系统监控（点击 Hero 右侧 CPU/内存圆环或按 `6`）**：90 秒 CPU / 物理内存渐变面积折线图（悬停回看）+ 双圆环硬件表 + 多端只读扫描与归档状态。
  7. **自定义日期与导出**：时间范围新增 **`自定义`**（输入起止 `YYYY-MM-DD` 筛选）；顶栏 **`导出`** 按钮支持原生保存框导出 **完整统计快照（JSON）**、**当前会话列表（UTF-8 BOM CSV）**、**每日汇总表（CSV）** 或复制 `/statusline` 配置片段。
- **快捷键**：`1`–`5` 切换标签、`6` 系统监控、`B` 预算与桥接、`/` 聚焦会话搜索、`D` 切换 `Tok`/`$`、`P` 脱敏模式、`T` 总计/仅增量、`↑`/`↓` 切换会话、`Backspace` 清除筛选、`R`/`F5` 刷新、`Ctrl+C` 复制摘要、`Esc` 退出输入框或隐藏面板。

## 官方 `/statusline` 桥接配置（可选）

若希望桌面版与 CLI 合规展示 Antigravity 官方剩余配额与重置倒计时，在桌面版点击「复制 `/statusline` 配置」（或运行 `.\agy-statusline.exe --print-config`），将生成的 `statusLine` 片段合并到 `~/.gemini/antigravity-cli/settings.json`（本工具遵守只读底线，不自动修改你的 `settings.json`）：

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

## 自定义单价与预算文件（位于程序同目录）

以下 JSON 文件均仅读写于 `agy-token-monitor.exe` 同目录，**绝不写入 `~/.gemini`**：

- `pricing.json`：自定义模型单价热覆盖（可在「模型 → 单价表」在线编辑，或手动编写，单位 USD / 1M tokens）：
  ```json
  {
    "models": {
      "gemini-4-neon": { "input": 3.0, "cache_read": 0.15, "output": 15.0 },
      "claude-opus-5-5": { "input": 4.0, "cache_read": 0.20, "cache_write": 5.0, "output": 20.0 }
    }
  }
  ```
- `budget.json`：5h / 每日 / 每周美元预算与预警/临界百分比（可在「模型 → 预算 / 桥接」在线编辑）。
- `statusline_state.json`：由 `agy-statusline.exe` 写入的官方配额脱敏缓存。
- `archive.json`：本地历史轮次轻量归档（可用 `--no-archive` 禁用）。

## 安全与只读保证

- **对 `~/.gemini` 绝对只读**：仅以只读方式打开 Antigravity 目录下的纯文本白名单文件：
  - `brain/<id>/.system_generated/logs/transcript.jsonl`（缺失时回退 `transcript_full.jsonl`）
  - `annotations/<id>.pbtxt`（会话标题）
  - `cache/conversation_metadata.json`（标题与工作区）
- **不碰 SQLite 数据库**：不打开 `conversations/*.db`，避免与活跃 `agy` 会话产生 WAL/SHM 锁冲突。
- **不读凭证、不联网、不改配置**：零网络请求、零账号令牌读取；Python CLI 通过单元测试中的 AST 静态检查强制保障零文件写操作与零网络模块导入。

## 快速开始

### 从源码编译（Rust GNU 工具链 + MinGW `windres`）

```powershell
cd rust
cargo test --all-targets
cargo build --release
copy target\release\agy-token-monitor.exe ..\agy-token-monitor.exe
copy target\release\agy-statusline.exe ..\agy-statusline.exe
```

### 运行命令

```powershell
# 启动桌面悬浮胶囊与系统托盘
.\agy-widget.cmd
# 常用参数：托盘悬停模式 / 极简角标 / 浅色纸感 / 费用视图 / 脱敏模式 / 禁用本地归档 / 指定副屏 / 指定单价表
.\agy-widget.cmd --tray-hover
.\agy-widget.cmd --mini --light --cost --redact
.\agy-widget.cmd --no-archive --monitor 1 --pricing .\pricing.json

# 生成官方 /statusline 配置片段
.\agy-statusline.cmd --print-config

# Python 零依赖命令行版（严格只读）
python agy_token_stats.py --today --human
python agy_token_stats.py --compact
python agy_token_stats.py --view blocks --human
python agy_token_stats.py --view tools --days 7
python agy_token_stats.py --view weekly --sort cost --human
python agy_token_stats.py --view all --redact
python agy_token_stats.py --format json --redact
# 脚本化预算检查（退出码：0 正常 / 10 预警 / 20 临界）
python agy_token_stats.py --budget-daily 30 --budget-5h 15 --check-budget
```
