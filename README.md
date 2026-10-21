# orangeDesk

> 一个低占用的开源「一体化桌面环境层」：一个进程、一套渲染管线、统一资源调度，
> 让动态壁纸、桌面小组件、任务栏透明、窗口材质共享同一个受控的合成与调度中心，
> 并在全屏、锁屏、电池、explorer 重启等边界场景下自动收缩资源。

设计文档：[orangedesk-plan-v2.md](./orangedesk-plan-v2.md)（§编号在本仓库代码注释中随处引用）。
Windows 单平台优先（§10），Rust 内核 + 原生渲染，Apache-2.0。

## 当前状态：v0.1 开发中（拓扑验证 + 内核骨架）

| v0.1 目标（文档 §6） | 状态 |
|---|---|
| 拓扑探测器（Classic WorkerW / Raised 24H2+ / 回退，0x052C + 重试） | ✅ `orange-wallpaper/src/topology.rs` |
| 壁纸最小实现：DirectComposition flip-model 提交 | 🟡 纯色 + 图片（WIC 解码 + D2D DrawBitmap + cover 缩放 + 失败降级纯色）已通；视频硬解待补 |
| 内核：单实例互斥、模块注册、生命周期事件（锁屏/全屏/电池） | ✅ `orange-kernel`（事件总线 + 注册中心 + 崩溃隔离 + supervisor） |
| 多显示器 / 混合 DPI 专项摸底 | ✅ `orange-wallpaper/src/monitor.rs`（枚举 + 每屏 DPI + 混合 DPI 风险检测，初始化时输出报告；per-monitor 独立壁纸仍属 v0.4） |
| 性能基准脚本就位 | ✅ `tools/perf-bench/`（采样 + 阈值占位 + CI 归档） |

图片格式说明：WIC 内置 JPG / PNG / BMP / TIFF / GIF；WebP / AVIF 依赖系统编解码扩展
（微软商店的 WebP / AV1 扩展），缺扩展时自动降级纯色并在日志给出原因（P3 不静默失败）。

## 架构（§2：微内核 + 可插拔能力模块 + 拓扑适配层）

```
apps/orange-desk        常驻主程序（单实例 → 配置 → 模块装配 → 事件循环）
crates/orange-kernel    微内核：只做总线 / 预算 / 注册 / 守护四件事
crates/orange-module-api 模块契约：IModule 生命周期、KernelEvent、资源预算
crates/orange-wallpaper 壁纸模块：拓扑适配 + DComp flip-model 渲染
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
