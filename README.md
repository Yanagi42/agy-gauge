# agy-gauge

一个**严格只读、极轻量**的 Antigravity CLI (`agy`) 本地 Token 用量与系统负载仪表盘（Windows）：

- **Rust + Direct2D 桌面悬浮版**（`agy-token-monitor.exe`，约 676 KB）：右下角逐像素抗锯齿毛玻璃胶囊（内置实时 CPU 迷你波形 + 圆环内存表），常驻内存仅 **~8–11 MB**；点击即开/再点即关 4 视图详情面板（独立进程，关闭后内存完全归零）。
- **Python 零依赖命令行版**（`agy_token_stats.py`）：终端表格 / JSON / CSV 快速统计与导出。

## 桌面版特性

- **双编辑级主题**：内置 **暖炭黑（Obsidian）** 与 **浅色纸感（Warm Paper）** 两套克制配色，自动调用 `Segoe UI Variable` / `Maple Mono` / `JetBrains Mono`，支持一键切换并同步 Win11 标题栏。
- **右下角轻量胶囊**：`UpdateLayeredWindow` 逐像素圆角与柔和阴影，不占任务栏/Alt-Tab、不抢焦点；支持**自由拖拽（跨屏记忆位置，右键可一键复位右下角）**；左键切换打开/关闭详情面板（详情窗同样不占任务栏图标），右键支持切换主题、极简角标、多屏停靠、始终置顶、K/M 格式与退出。
- **偏好持久化与双向同步**：主题、格式、屏幕与模式偏好自动保存至 `HKCU\Software\AgyTokenMonitor`，胶囊与详情页实时双向同步。
- **4 大详情视图**：
  1. **概览**：2×2 Bento 布局（14 天每日趋势 + 24 小时活跃节律热力柱 + 模型占比 + 最近活跃会话 + 本周 ISO 简报与实时波形）。
  2. **按天 & 按周**：按天 / ISO 自然周（周一~周日，含周环比与日均）/ 按月堆叠柱状图与可滚动明细表，支持**「总计 ↔ 仅增量」**一键切换，避免高缓存命中率淹没真实新增消耗。
  3. **会话追踪**：按时间/总量/输出/峰值上下文排序，点击任意会话查看**逐轮上下文增长剖析图**。
  4. **系统监控**：90 秒 CPU 与物理内存渐变面积折线图（支持鼠标悬停垂直准星回看历史任意秒）+ 双圆环硬件表 + 只读扫描状态。

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
