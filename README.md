# agy-gauge

一个**严格只读、极轻量**的 Antigravity CLI (`agy`) 本地 Token 用量、API 费用估算（USD）与系统负载仪表盘（Windows）：

- **Rust + Direct2D 桌面悬浮版**（`agy-token-monitor.exe`，约 700 KB）：右下角逐像素抗锯齿毛玻璃胶囊（内置实时 CPU 迷你波形 + 圆环内存表，支持自定义组合显示今日总 Token / 今日估算费用 `$` / 非缓存增量 / 命中率 / 上下文）；左键单击即时唤出/隐藏详情面板（进程内复用热缓存与 90 秒系统负载历史，无冷启动扫描与过渡动画延迟）。
- **Python 零依赖命令行版**（`agy_token_stats.py`）：终端表格 / JSON / CSV 快速统计、费用估算与导出。

## 桌面版特性

- **双编辑级主题**：内置 **暖炭黑（Obsidian）** 与 **浅色纸感（Warm Paper）** 两套克制配色，自动调用 `Segoe UI Variable` / `Maple Mono` / `JetBrains Mono`，支持一键切换并同步 Win11 标题栏。
- **全局 `Tok | $` 费用视图切换（快捷键 `D`）**：
  - 顶栏一键在 **Token 用量** 与 **估算 API 费用（USD）** 之间切换（偏好持久化至注册表）；Token 模式下 Hero 与明细表同样常驻显示 `$` 估算金额。
  - 费用模式下，趋势柱图、按天/按周/按月与逐轮剖析全部按美元拆解堆叠（缓存读费用 / 新输入费用 / 输出费用），并实时计算**缓存命中已省美元金额**与周环比费用变化；模型/会话均支持按费用排序。
  - 内置主流 Gemini（含 Gemini 4 Neon / Carbon / Hydrogen、Gemini 3.8 / 3.1 / 2.5）、Claude（含 Opus / Sonnet / Haiku 5.5、5、4.x）及 OpenAI 官方公开 API 单价表；上下文压缩 `compaction` 自动按所属会话主模型计价。
  - 支持**严格只读 `pricing.json` 本地热覆盖**：在程序同目录放置 `pricing.json`（或通过 `--pricing <path>` 指定）即可覆盖或补充自定义模型单价，无需重新编译、修改后自动热重载。
- **右下角轻量胶囊**：`UpdateLayeredWindow` 逐像素圆角与柔和阴影，不占任务栏/Alt-Tab、不抢焦点；支持**自由拖拽（跨屏记忆位置，外接屏热插拔自动重新停靠，右键可一键复位右下角）**；左键秒开/隐藏详情面板（打开时边框微亮反馈，详情窗同样不占任务栏图标）；右键「胶囊显示内容」支持自由组合勾选**今日总 Token / 今日估算费用 ($) / 非缓存增量 Δ / 缓存命中率 % / 活跃会话上下文 Ctx（逼近 400K 黄点提醒）/ CPU 波形 / 内存环**，以及主题、多屏停靠、始终置顶、K/M 格式与退出。
- **偏好持久化与双向同步**：主题、格式、费用视图模式、时间范围（默认「今日」，与悬浮胶囊一致）、胶囊组合、屏幕与位置偏好自动保存至 `HKCU\Software\AgyTokenMonitor`，胶囊与详情页实时双向同步。
- **扁平 5 视图 + 交互式钻取 + 键盘快捷键**：
  1. **概览**：三区 Bento 布局（智能趋势图：今日自动切分时柱图、多日自动切近 7–14 天 + 可点击跳转的本周 vs 上周 ISO 对比 + **「模型分布 | 项目分布」**一键切换，**点击任意模型/项目/日期柱可直接钻取至对应会话** + 底部全宽最近活跃会话表，展示单会话缓存命中率、估算费用与 `峰值Ctx`）。
  2. **按天**：每日堆叠柱状图与可滚动明细表（点击柱体或行直接按日期筛选会话），支持**「总计 ↔ 仅增量」**与**「Tok ↔ $」**双维度切换。
  3. **按周**：ISO 自然周（周一~周日，含周环比、活跃天、日均与估算费用，点击周柱或行可按周筛选会话），内置「按周 (ISO) | 按月」快速切换。
  4. **模型**：内置「**缓存对比 | 单价表 ($)**」子视图——按模型名实际宽度自适应排版（完整展示如 `gemini-4-hydrogen-tiered-xhigh` 等长模型名），既可横向对比各模型缓存命中率（支持按总量 / 按费用 / 命中率排序），也可直接在面板内查看完整 API 价格表，点击任意模型或 `+ 新增模型` **在线修改/设置输入、缓存读、缓存写、输出单价（USD / 1M tokens）或一键恢复默认**，保存后写入程序目录 `pricing.json` 并立即重算全盘费用（精确模型名优先于前缀匹配；对 `~/.gemini` 始终保持绝对只读）。
  5. **会话**：自动识别 `is_internal` 子代理（Subagent）会话，从启动 `SYSTEM_MESSAGE` 提取任务标题并关联父会话、继承父会话所属工作区（消除 `(未命名会话)` 与 `(未知工作区)`）；默认开启 **`子会话:归组`**（汇总到父任务并标注 `(+N子)`，可一键切为 **`子会话:独立`** 或在剖析卡点击 `查看N子会话 ›` / `查看所属任务 ›` 钻取）。同时展示**单会话缓存命中率、估算费用**与钻取筛选标签（点击 `×` 或按 `Backspace` 清除），支持按最近 / **费用** / 非缓存 / 总 Token / **命中低→高** / `峰值Ctx` 排序，点击任意会话（或用 `↑`/`↓` 切换）查看**逐轮上下文/费用剖析图**（含所属项目名、压缩标记点与单会话明细一键复制）。
  6. **系统监控（点击顶部概览条右侧 CPU/内存圆环或按 `6` 切换）**：90 秒 CPU 与物理内存渐变面积折线图（悬停垂直准星回看）+ 双圆环硬件表 + 只读扫描状态。
  - **快捷键**：`1`–`5` 切换标签、`6` 切换系统监控、`D` 切换 Token/费用(`$`)视图、`T` 切换总计/仅增量、`↑`/`↓` 切换会话、`Backspace` 清除筛选、`R`/`F5` 刷新、`Ctrl+C` 复制摘要、`Esc` 隐藏面板。

## 自定义单价（可选 `pricing.json`）

如需调整单价或补充新模型，可在 `agy-token-monitor.exe`（或 `agy_token_stats.py`）同目录创建 `pricing.json`（程序严格只读，不自动生成；单位：USD / 1M tokens）：

```json
{
  "models": {
    "gemini-4-neon": { "input": 3.0, "cache_read": 0.15, "output": 15.0 },
    "claude-opus-5-5": { "input": 4.0, "cache_read": 0.20, "cache_write": 5.0, "output": 20.0 }
  }
}
```

## 安全与只读保证

- **对 `~/.gemini/antigravity-cli` 绝对只读**：仅扫描纯文本白名单文件：
  - `brain/<id>/.system_generated/logs/transcript.jsonl`（缺失时回退 `transcript_full.jsonl`）
  - `annotations/<id>.pbtxt`（会话标题）
  - `cache/conversation_metadata.json`（标题与工作区）
- **不碰 SQLite 数据库**：不打开 `conversations/*.db`，避免与活跃 `agy` 会话产生 WAL/SHM 锁冲突。
- **不读凭证、不联网、零外部子进程**：CPU/内存直接通过 Win32 `GetSystemTimes` / `GlobalMemoryStatusEx` / `K32GetProcessMemoryInfo` 采样，文件扫描带内存 `(mtime, size)` 增量缓存。

## 快速开始

### 从源码编译桌面版（Rust GNU 工具链 + MinGW `windres`）

```powershell
cd rust
cargo test
cargo build --release
copy target\release\agy-token-monitor.exe ..\agy-token-monitor.exe
```

### 运行命令

```powershell
# 启动右下角悬浮监控
.\agy-widget.cmd
# 极简角标 / 浅色纸感 / 直接以费用视图打开 / 指定副屏 / 指定单价表
.\agy-widget.cmd --mini
.\agy-widget.cmd --light
.\agy-widget.cmd --cost
.\agy-widget.cmd --monitor 1
.\agy-widget.cmd --pricing .\pricing.json

# Python 零依赖命令行版
python agy_token_stats.py --today --human
python agy_token_stats.py --view weekly
python agy_token_stats.py --sort cost --human
python agy_token_stats.py --session <id-prefix>
python agy_token_stats.py --format json
```
