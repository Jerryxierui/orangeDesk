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

## 架构（§2：微内核 + 可插拔能力模块 + 拓扑适配层）

```
apps/orange-desk        常驻主程序（单实例 → 配置 → 模块装配 → 事件循环）
crates/orange-kernel    微内核：只做总线 / 预算 / 注册 / 守护四件事
crates/orange-module-api 模块契约：IModule 生命周期、KernelEvent、资源预算
crates/orange-wallpaper 壁纸模块：拓扑适配 + DComp flip-model 渲染
crates/orange-taskbar   任务栏透明模块：composition API + 规则引擎 + 能力降级
crates/orange-widgets   小组件系统：IWidget 契约 + EDF 调度器 + 崩溃隔离
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
