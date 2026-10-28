# orangeDesk

> 一个低占用的开源「一体化桌面环境层」：一个进程、一套渲染管线、统一资源调度，
> 让动态壁纸、桌面小组件、任务栏透明、窗口材质共享同一个受控的合成与调度中心，
> 并在全屏、锁屏、电池、explorer 重启等边界场景下自动收缩资源。

设计文档：[orangedesk-plan-v2.md](./orangedesk-plan-v2.md)（§编号在本仓库代码注释中随处引用）。
Windows 单平台优先（§10），Rust 内核 + 原生渲染，Apache-2.0。

## 当前状态：v0.1 代码完成 → v0.2 进行中（任务栏透明 + 性能仪表盘）

**v0.3 预研已落地**（§3 模块 B 跨平台核心）：
- `orange-widgets`：`IWidget` 契约（独立更新频率 / 遮挡策略声明）、EDF 调度器
  （最早截止优先；暂停恢复后追赶一轮即回正常节奏，不连续补跑）、
  单组件崩溃 catch_unwind 隔离（其他组件无感）、锁屏/节能/全屏全组件暂停；
  首发组件前两个：时钟（1s）、系统监控（2s，Linux /proc 完整实现）
- Windows 渲染窗口（D2D）与遮挡检测（WindowFromPoint / DWM 事件）留待真机阶段，
  数据层已可在线运行（demo / CI 链路验证）

**v0.2 已落地**（§3 模块 C + §6 v0.2）：
- `orange-taskbar`：`SetWindowCompositionAttribute` 主路径（运行时动态解析，
  undocumented API）、能力探测降级链（Acrylic → Blur → Clear → Native，
  原因用户可见）、声明式规则引擎（条件 → 材质，默认规则开箱即用、配置可整体覆盖）、
  事件驱动评估（前台进程 / 全屏 / 电池节能 / 锁屏）、explorer 重启自动重找窗口重应用、
  finalize 恢复系统原生（卸载零残留）；预算 0.5% CPU / 15MB
- 性能仪表盘数据层（§5/P6）：内核进程级采样（Windows GetProcessTimes/MemoryInfo +
  Linux /proc）、`process.total` 全产品预算（§4 空闲行）接入预算管理器——
  持续超标自动广播 `BudgetExceeded`；`ModuleStatus` 事件后刷新，
  session summary 按模块输出材质 / 降级说明
- 仪表盘数据接口：supervisor 每 2s 原子写 `<config_dir>/status.json`（模块降级状态 +
  进程读数 + 预算表含 verdict），`orange-desk --status` 读出（陈旧快照 >10s 警告）
- 组件数据层补全（§3 B-3）：天气（open-meteo 解析 + WMO 码表，Provider trait 待真机接
  WinHTTP）、媒体控制（MediaSource trait 待接 SMTC）、待办（JSON 原子存储，SQLite 迁移留口）
- §5 降级矩阵首行落地：`BudgetExceeded`（process.total / wallpaper）→ 壁纸帧率
  60→30→15→静态快照（暂停 + Trim），运行时跨线程调档，状态用户可见
- v0.2 剩余：设置面板卫星进程（WinUI 3 / WPF，Windows 真机环境）与仪表盘 UI

| v0.1 目标（文档 §6） | 状态 |
|---|---|
| 拓扑探测器（Classic WorkerW / Raised 24H2+ / 回退，0x052C + 重试） | ✅ `orange-wallpaper/src/topology.rs` |
| 壁纸最小实现：DirectComposition flip-model 提交 | ✅ 纯色 + 图片（WIC + D2D DrawBitmap + cover 缩放）+ 视频（MF 硬解 + GPU VideoProcessor blit + 帧率档位 + 循环播放），失败均降级纯色并记录原因 |
| 内核：单实例互斥、模块注册、生命周期事件（锁屏/全屏/电池） | ✅ `orange-kernel`（事件总线 + 注册中心 + 崩溃隔离 + supervisor） |
| 多显示器 / 混合 DPI 专项摸底 | ✅ `orange-wallpaper/src/monitor.rs`（枚举 + 每屏 DPI + 混合 DPI 风险检测，初始化时输出报告；per-monitor 独立壁纸仍属 v0.4） |
| 性能基准脚本就位 | ✅ `tools/perf-bench/`（采样 + 阈值占位 + CI 归档） |

v0.1 代码级目标全部完成；剩余验收项：真实 Windows 机器上的三版本（Win10 22H2 /
Win11 23H2 / 24H2）拓扑不黑屏验证与视频路径真机调优（色彩空间显式设置等，见
`video.rs` 模块注释的留白清单）。

图片格式说明：WIC 内置 JPG / PNG / BMP / TIFF / GIF；WebP / AVIF 依赖系统编解码扩展
（微软商店的 WebP / AV1 扩展），缺扩展时自动降级纯色并在日志给出原因（P3 不静默失败）。

壁纸播放列表（§13 F1，Wallpaper Engine 同款能力）——`modules.wallpaper.playlist`：

```json
{
  "playlist": {
    "entries": ["C:/walls/dawn.jpg", "D:/videos/rain.mp4", "C:/walls/night.png"],
    "interval_min": 30,
    "order": "random"
  }
}
```

条目按扩展名自动识别图片 / 视频；顺序（sequential）或随机（random）循环；
锁屏 / 全屏 / 节能期间到期的切换自动延迟到恢复后执行（不复活暂停的渲染）。

多显示器独立壁纸（§3 A-3 / F1b）：每显示器一个挂载 + 渲染实例（混合 DPI 各自矩形），
按设备名覆盖配置（覆盖式，未列出的显示器用全局配置）：

```json
{
  "mode": "video", "video_path": "D:/w/rain.mp4",
  "per_monitor": { "\\.\DISPLAY2": { "mode": "image", "image_path": "C:/w/focus.jpg" } }
}
```

单显示器实例失败不拖垮其他显示器；皮肤 / 场景中的 wallpaper 段自动继承该能力。

声明式皮肤（§13.2 L2）——一个 YAML 文件 = 一套完整桌面（壁纸 + 播放列表 +
任务栏规则 + 组件开关 + v0.4 材质预留），见 [samples/midnight.yaml](./samples/midnight.yaml)：

```bash
orange-desk --skin samples/midnight.yaml   # 应用并持久化（config.json active_skin）
```

皮肤各段直接复用模块的配置 schema（零漂移）；出现的段覆盖同名模块配置，
未出现的段保留用户设置（局部换装合法）；解析 / 校验错误全部即时可读。

场景模式（§13.2 L3 / F3b）——多套皮肤组合 + 触发器自动切换（运行时热应用，
进程不重启）。把 [samples/scenes.yaml](./samples/scenes.yaml) 放到配置目录：

```bash
orange-desk --scene game      # 手动激活并持久化（active_scene）
# 触发器自动切换：前台进程匹配（如启动 Cyberpunk2077 → game 场景）、
# 每日时间表（22:30 → night 场景）
```

场景切换 = 内核对各模块下发 `ModuleConfigChanged` 热配置事件：壁纸 diff 后
仅调档或换源、任务栏重建规则即时重评估、组件集合热重载（失败自动回滚旧配置）——
这也是配置热重载的公共地基（F9 热重载的机制已就位）。

音频响应（§13 F4，Wallpaper Engine 音频可视化同款）：`orange-audio` 模块经
WASAPI loopback 抓取系统混音（事件驱动、零轮询），FFT → 16 段对数频谱 →
`AudioSpectrum` 事件以 ~30Hz 广播（EMA 平滑、静音停发）；首个消费者
`spectrum` 组件已内置（unicode 条形图，D2D 渲染层上线后升级为柱状图），
壁纸渲染响应（音频可视化叠加）待真机阶段接入。预算 0.3% CPU / 10MB。

悬浮 Dock（§13 F7，Nexus/Seelen 式启动器；**新增**顶层窗口，不替换任务栏）：
数据层就绪——`modules.dock` 配置停靠边/对齐/尺寸/透明度/自动隐藏 + 固定项
（应用路径、shell:URI、协议链接均可作 target）；运行状态合并（exe 基名大小写
不敏感匹配、前台激活高亮）；启动命令构造（引号感知参数分词）。DComp 渲染
窗口（圆角半透明 + 图标 + 放大动效）、进程枚举、ShellExecuteW 启动在真机阶段
接入；皮肤（L2）与场景（L3）的 dock 段已自动生效。

灵动岛（Dynamic Island 式活动胶囊）：顶部胶囊窗口按"活动"变形——收缩态显示
最高优先级活动的单行摘要（媒体 ♪ / 倒计时 ⏱ / 专注 ◐ / 频谱条形），展开态
显示详情卡片（进度 / 宽频谱），超时自动收回。活动优先级：倒计时 > 播放中媒体
> 暂停媒体 > 专注 > DND > 频谱；媒体源接 `MediaSource`（真机 SMTC），频谱接
`AudioSpectrum` 事件，定时器 / 专注入口随托盘与快捷指令接入。配置
`modules.island`（位置 / 自动收回时长 / 活动启停），皮肤与场景段已生效；
DComp 胶囊渲染窗口（形变动画）在真机阶段接入。

## 架构（§2：微内核 + 可插拔能力模块 + 拓扑适配层）

```
apps/orange-desk        常驻主程序（单实例 → 配置 → 模块装配 → 事件循环）
crates/orange-kernel    微内核：只做总线 / 预算 / 注册 / 守护四件事
crates/orange-module-api 模块契约：IModule 生命周期、KernelEvent、资源预算
crates/orange-wallpaper 壁纸模块：拓扑适配 + DComp flip-model 渲染
crates/orange-taskbar   任务栏透明模块：composition API + 规则引擎 + 能力降级
crates/orange-widgets   小组件系统：IWidget 契约 + EDF 调度器 + 崩溃隔离
crates/orange-audio     音频响应：WASAPI loopback + FFT 频谱广播
crates/orange-dock      悬浮 UI 家族：Dock（新增启动器，非任务栏替换）+ 灵动岛
crates/orange-skin      声明式皮肤层（L2）：一个 YAML = 一套完整桌面
tools/perf-bench        性能采样脚本（CI 门禁数据源）
```

内核纪律的落点：
- 模块实现 `Module`（`initialize / on_event / suspend / resume / finalize` + 预算声明）
- 模块 panic / Crash → 内核 catch_unwind 隔离，其他模块无感（P3）
- 全屏 / 锁屏 / 电池节能 → 事件驱动完全暂停（不是降帧）+ `IDXGIDevice3::Trim`
- explorer 重启（TaskbarCreated）→ 壁纸自动重挂载
- 一切低频状态变化走 `KernelEvent`，无高频轮询（P4）

## 开发

```bash
cargo build --workspace          # 当前平台编译
cargo test --workspace           # 内核逻辑 / 契约 / 降级路径测试
cargo run -p orange-desk -- --demo-events   # 注入事件序列验证模块生命周期链路
```

Windows API 侧代码用 `cfg(windows)` 隔离，交叉检查：

```bash
rustup target add x86_64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc -p orange-wallpaper -p orange-kernel
```

## 性能预算（§4，CI 门禁）

| 指标 | 目标 |
|---|---|
| 空闲常驻 CPU / 内存 | < 0.8% / < 60MB |
| 壁纸播放 CPU（1080p 30fps 硬解） | < 3% |
| 壁纸播放内存（全产品） | < 120MB |
| 全屏 / 锁屏 / 电池节能 | 壁纸 GPU 占用归零（暂停 + Trim） |

运行 `tools/perf-bench/sample.ps1` 采样；阈值接 CI 门禁在基线建立后启用（D6）。

## 路线图

v0.1 拓扑验证 + 内核骨架 → v0.2 任务栏透明 + 性能仪表盘 → v0.3 小组件系统 →
v0.4 窗口材质 + 多显示器/混合 GPU → v1.0 稳定性打磨发布 → v1.1+ 声明式生态。
详见设计文档 §6。
