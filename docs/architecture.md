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
    radio/                 射频协议：ble/ble_ad/zigbee/zigbee_check
    net/                   Wi-Fi
    sys/                   芯片服务：system/temperature
  gateway/                 产品领域与能力服务
    device.rs              物模型：Protocol/Property/Value
    event.rs               网关事件：GatewayEvent（设备身份、在场、属性）
    bus.rs                 UI 最新快照和独立上云队列
    command/             set_power 入口及持久化结果交付
    identity.rs            gateway_id、boot_counter、boot_id、seq
    presence.rs            设备在线判定（纯逻辑，可宿主测试）
    access/                接入能力
      zigbee.rs            Zigbee 协调器服务：入网、发现、上报、在场
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
| M1（已完成） | Zigbee 接入观察、身份、UI 状态页 | `gateway/access/zigbee.rs`、`gateway/identity.rs`、`ui/` |
| M2 | Zigbee 本地 `set_power`、回读确认、命令去重与结果交付 | `gateway/command/`、`gateway/access/zigbee/` |
| M3 | Wi-Fi + MQTT 上云：events/status/commands/results、心跳、断网补传 | `gateway/uplink/`、`drivers/net/wifi.rs` |
| M4 | SD 上的 `rules.toml` 本地 TCA、离线队列持久化 | `gateway/rules/`、`gateway/storage/`、`drivers/board/sdcard.rs` |
| M5 | BLE 执行设备接入与 GATT 控制 | `gateway/access/ble.rs`、`drivers/radio/ble.rs` |
| 贯穿 | 掉电保持：mains-loss 检测 + 快速保存窗口、按键交互、配网 | `gateway/power.rs`、`ui/`、`drivers/board/power.rs` |

未落地的能力只在文档中占位，不在 `src/` 里建空模块；实现时按上表落点新增目录。
