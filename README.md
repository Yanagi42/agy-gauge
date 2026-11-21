# agy-gauge

一个**严格只读、极轻量**的 Antigravity CLI (`agy`) 本地 Token 用量与系统负载仪表盘（Windows）：

- **Rust + Direct2D 桌面悬浮版**（`agy-token-monitor.exe`，约 680 KB）：右下角逐像素抗锯齿毛玻璃胶囊（内置实时 CPU 迷你波形 + 圆环内存表）；左键单击即时唤出/隐藏详情面板（进程内复用热缓存与 90 秒系统负载历史，无冷启动扫描与过渡动画延迟）。
- **Python 零依赖命令行版**（`agy_token_stats.py`）：终端表格 / JSON / CSV 快速统计与导出。

## 桌面版特性

- **双编辑级主题**：内置 **暖炭黑（Obsidian）** 与 **浅色纸感（Warm Paper）** 两套克制配色，自动调用 `Segoe UI Variable` / `Maple Mono` / `JetBrains Mono`，支持一键切换并同步 Win11 标题栏。
- **右下角轻量胶囊**：`UpdateLayeredWindow` 逐像素圆角与柔和阴影，不占任务栏/Alt-Tab、不抢焦点；支持**自由拖拽（跨屏记忆位置，右键可一键复位右下角）**；左键切换打开/关闭详情面板（详情窗同样不占任务栏图标），右键支持切换主题、极简角标、多屏停靠、始终置顶、K/M 格式与退出。
- **偏好持久化与双向同步**：主题、格式、时间范围（默认「今日」，与悬浮胶囊一致）、屏幕与位置偏好自动保存至 `HKCU\Software\AgyTokenMonitor`，胶囊与详情页实时双向同步。
- **扁平 4 视图 + 交互式钻取 + 键盘快捷键**：
  1. **概览**：三区 Bento 布局（智能趋势图：今日自动切分时柱图、多日自动切近 7–14 天 + 可点击跳转的本周 vs 上周 ISO 对比 + **「模型分布 | 项目分布」**一键切换，**点击任意模型/项目/日期柱可直接钻取至对应会话** + 底部全宽最近活跃会话表，展示单会话缓存命中率与 `峰值Ctx`）。
  2. **按天**：每日堆叠柱状图与可滚动明细表（点击柱体或行直接按日期筛选会话），支持**「总计 ↔ 仅增量」**一键切换，高亮总 Token 与命中率。
  3. **按周**：ISO 自然周（周一~周日，含周环比、活跃天与日均，点击周柱或行可按周筛选会话），内置「按周 (ISO) | 按月」快速切换。
  4. **会话**：展示**单会话缓存命中率**与钻取筛选标签（点击 `×` 或按 `Backspace` 清除），支持按最近 / 非缓存 / 总 Token / **命中低→高** / `峰值Ctx` 排序，点击任意会话（或用 `↑`/`↓` 切换）查看**逐轮上下文剖析图**（含所属项目名、压缩标记点与单会话明细一键复制）。
  5. **系统监控（点击顶部概览条右侧 CPU/内存圆环或按 `5` 切换）**：90 秒 CPU 与物理内存渐变面积折线图（悬停垂直准星回看）+ 双圆环硬件表 + 只读扫描状态。
  - **快捷键**：`1`–`4` 切换标签、`5` 切换系统监控、`T` 切换总计/仅增量、`↑`/`↓` 切换会话、`Backspace` 清除筛选、`R`/`F5` 刷新、`Ctrl+C` 复制摘要、`Esc` 隐藏面板。

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
# 极简角标 / 浅色纸感 / 指定副屏
.\agy-widget.cmd --mini
.\agy-widget.cmd --light
.\agy-widget.cmd --monitor 1

# Python 零依赖命令行版
python agy_token_stats.py --today --human
python agy_token_stats.py --view weekly
python agy_token_stats.py --session <id-prefix>
python agy_token_stats.py --format json
```
