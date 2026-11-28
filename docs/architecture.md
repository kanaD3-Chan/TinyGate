# Rust 架构：按产品能力组织（2026-11-21）

> 本文回答“新代码放哪里、依赖谁能指向谁”。历史设计演进见 [DESIGN.md](../DESIGN.md)。

## 一句话

`app` 负责装配，`gateway` 负责产品能力，`drivers` 只做 C 适配，`ui` 是唯一屏幕写者。

早期目录按“验证 bench 用到了哪些外设/协议”切分（`drivers/`、`radio/`、`platform/` 三套平行外壳）。现在按产品主线切分：**接入设备 → 维护设备模型与在线状态 → 本地自动化 → 上云 → 展示与电源**。外设属于实现细节，统一收进 `drivers/`。

## 目录与职责

```text
src/
  main.rs                  启动 ESP-IDF 运行时，调用 app::run()
  lib.rs                   模块清单与分层规则
  app/                     组合根：只做装配，不放业务规则
    mod.rs                 board + identity + 双 executor + 服务注册
    board.rs               板级探测，逐项降级为 Option
    runtime.rs             critical-section 的 FreeRTOS 实现
  drivers/                 C 适配层：tg_* 的薄封装与所有权守卫
    lease.rs               单所有者 Lease + C 错误码映射
    board/                 板载外设：button/rgb/display/sdcard/power
    radio/                 射频协议：ble/ble_ad/zigbee/zigbee_event
    net/                   Wi-Fi
    sys/                   芯片服务：system/temperature
  gateway/                 产品领域与能力服务
    device.rs              物模型：Protocol/Property/Value
    event.rs               网关事件：GatewayEvent（设备身份、在场、属性）
    bus.rs                 UI 最新快照和独立上云队列
    command/               set_power 入口及持久化结果交付
    identity.rs            gateway_id、boot_counter、boot_id、seq
    access/                接入能力
      zigbee/              Zigbee 协调器服务，含发现、上报、在场与控制
  ui/                      展示层：唯一允许写 OLED 的地方
    console.rs             状态页行缓冲与分页刷新（后端无关）
    icon.rs                图标词汇
    macros.rs              print!/println!/print_at!/println_at!
    view.rs                事件 → 屏幕文本
    task.rs                低优先级 UI 任务
```

## 依赖方向

```text
main ──> app ──> gateway ──> drivers
          └────> ui ──────> drivers (仅 display)
```

- `drivers/` 不依赖 `gateway/`、`ui/`、`app/`。
- `gateway/` 只能通过 `drivers/` 访问硬件。设备身份与命令模块通过共享的 esp-idf-svc NVS 服务持久化业务数据。
- `ui/` 只能通过 `drivers/board/display` 拿屏幕，且 `Display` 只由 `Ui::new` 构造。
- `app/` 可以依赖所有层，但只做装配与生命周期管理。

## 责任边界与测试落点（2026-11-28）

按仓库路径划分，不以“这次改动涉及哪个功能”划分。设计依据见 [DESIGN.md](../DESIGN.md) 同日条目。

| 路径 | 负责人 | 可改内容 |
| --- | --- | --- |
| `src/**` | Rust 主线（kanaDE） | 业务、门面、Rust 单测；**C 侧不改任何文件** |
| `Cargo.toml`、`Cargo.lock`、`build.rs`、`.cargo/**`、`rust-toolchain.toml` | Rust 主线 | 构建与依赖 |
| `tests/zigbee-host/**` | Rust 主线 | 宿主测试夹具；C 侧可运行，改动由 Rust 侧做 |
| `components/**` | C 组件（Reacher） | C 实现、公开头、CMake/Kconfig、组件依赖 |
| `components/tg_display/src/display_canvas.c` | 共享（图标豁免） | Rust 侧仅可改 `icons[]` 图标半字形与码位注释；`glyph()`、字体表、`tg_canvas_*` 仍归 C |
| `components/<name>/test/**`、`tests/*_test.c` | C 组件 | C 驱动测试 |
| `contracts/**`、`DESIGN.md`、`docs/architecture.md` | 共享 | 跨边界变更需双方确认 |

测试不跨边界借宿：

- C 驱动测试留在 C 侧（设备端 Unity 或主机 gcc），不得写进 `src/**` 的 `#[cfg(test)]` 模块、mock 或 fixture，也不得改写 `tests/zigbee-host/**`；测试 C 组件不是修改 Rust 源码的理由。
- Rust 业务测试留在 `src/**` 单测或 `tests/**` 宿主测试；mock 的是 `drivers` 门面，不是 C 源码。
- `tg_*.h` / `platform_bindings.h` 是 ABI 边界。改头必须同时给 Rust `extern "C"` 与门面适配，单独改头导致编译失败不算完成。
- 一个 PR 只动一侧；跨边界改动显式标注并互审，禁止顺带格式化或重排对方目录。
- 唯一例外是图标：Rust 侧可为加图标改 `display_canvas.c` 的 `icons[]` 并同步 `src/ui/icon.rs`；越过 `0x8F` 码位时由 C 侧调整 `tests/display_canvas_test.c` 的空白约定，Rust 侧不改该测试。

## 单一所有者与串行契约

每个 C 组件在固件生命周期内只允许一个 Rust 所有者：

- 所有权由 `drivers::lease::Lease` 的原子守卫保证，重复初始化返回错误。
- “同一时间只有一个调用者”由任务结构保证：一个服务任务持有自己的驱动，跨 `await` 持有；不许把驱动共享给多个任务。
- 释放失败时保留所有权，避免第二个所有者接管半释放的 C 子系统。

## 事件流

```text
C 事件队列 ──> Zigbee 服务 ──> GatewayEvent ──┬─> UI 最新快照
       命令入口 ───┘                         └─> 独立上云观测队列
                   └─> NVS 命令记录 ──> 待交付结果
```

短地址、ZCL 事务号和 C 队列溢出恢复由 Zigbee 服务处理。上云队列满时丢弃新观测并增加 `uplink_dropped()`，UI 快照继续更新。命令发出之前持久化受理状态，终态先落盘，再由调用方通过 `pending_results()` 读取、在交付成功后调用 `acknowledge()`。确认旧状态不会移除更新的终态。

调用方可使用 `command::submit_local` 发起带单调时钟超时的本地控制。后端命令使用 `command::submit` 和 UTC 到期时间，接入方在对时后调用 `set_network_time`。NVS 日志最多保留 64 条命令，每条新记录最多 148 字节，并兼容读取旧 JSON 记录。云端命令按契约至少保留到到期后 10 分钟，本地命令在本地截止时间后保留 60 秒。只有已确认交付且超过保留期的终态才能复用槽位，所有槽位仍有保留要求时返回 `SubmitError::Full`。记录更新走 ESP-IDF 的同键版本更新。MQTT 发布、回执确认和时钟同步由上云服务接入。

## 掉电保存契约

超级电容不是电池，不承担续航：

- 驱动只回答两件事：主电源是否丢失、保持电压还剩多少（用于估算保存窗口）。
- 掉电后的动作顺序是：停止射频/上云等非必要工作 → 按优先级落盘 → 停止业务并等待电源耗尽。
- 恢复供电按冷启动处理，不恢复未完成的即时控制。
- 保存内容由存储/命令模块决定，电源驱动不决定业务写什么。
- 保持窗口未实测标定前，不得在 UI 或日志里承诺具体毫秒数，也不得把“已发起保存”说成“已保证落盘”。

## 产品里程碑与落点

| 里程碑 | 能力 | 代码落点 |
| --- | --- | --- |
| M1（已完成） | Zigbee 接入观察、身份、UI 状态页 | `gateway/access/zigbee/`、`gateway/identity.rs`、`ui/` |
| M2 | Zigbee 本地 `set_power`、回读确认、命令去重与结果交付 | `gateway/command/`、`gateway/access/zigbee/` |
| M3 | Wi-Fi + MQTT 上云：events/status/commands/results、心跳、断网补传 | `gateway/uplink/`、`drivers/net/wifi.rs` |
| M4 | SD 上的 `rules.toml` 本地 TCA、离线队列持久化 | `gateway/rules/`、`gateway/storage/`、`drivers/board/sdcard.rs` |
| M5 | BLE 执行设备接入与 GATT 控制 | `gateway/access/ble.rs`、`drivers/radio/ble.rs` |
| 贯穿 | 掉电保持：mains-loss 检测 + 快速保存窗口、按键交互、配网 | `gateway/power.rs`、`ui/`、`drivers/board/power.rs` |

未落地的能力只在文档中占位，不在 `src/` 里建空模块；实现时按上表落点新增目录。
