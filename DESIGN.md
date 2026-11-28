# TinyGate 设计文档

##  食用方法：

每一条设计标明时间，废弃设计用 ~~删除线~~划掉留痕。

## example：

### 2026-11-10 示例标题

~~打算加上叮咚机打勾胶~~[2026-11-11]前面设计废除，因为出现了见缸🐎。



---

### 2026-11-10 基本设计框架

#### 1. 项目定位
基于 ESP32-C6 的多协议边缘物联网网关，实现低功耗蓝牙 (BLE) 与 Zigbee 设备接入，通过 Wi-Fi 接入云端，并提供边缘与云端双自动化控制能力。

#### 2. 硬件系统设计
- 核心主控: ESP32-C6 芯片，单芯片负责 Wi-Fi 6、BLE 5.3 以及 IEEE 802.15.4 (Zigbee 3.0) 射频时分共存。
- 电源与 UPS: ~~18650 锂电池配合动态电源路径管理芯片，实现外接供电与电池无缝切换。配备主电源掉电中断检测 GPIO 与电池电压 ADC 采样。~~ [2026-11-21] 改为超级电容掉电保持，只负责快速保存窗口；见下方“电源改为超级电容保持”。
- 本地存储: MicroSD 卡 (SPI 接口)，挂载 FATFS，用于持久化存储本地 rules.toml 规则文件、系统运行日志以及断网数据队列。
- 人机交互: 
  - 0.96 寸 OLED 显示屏 (I2C 接口，SSD1306)，显示 IP 地址、网络状态、设备数量与联动事件流。
  - 物理功能按键，用于配网触发、页面切换或规则重载。

#### 3. 网络与协议架构
- 感知层接入:
  - Zigbee: 网关作为协调器 (Coordinator) 组网，测试端利用另一块 ESP32-C6 通过多 Endpoint 技术模拟复合传感器。
  - BLE: 网关作为 Central/Observer，通过手机或多播工具模拟多个不同广播与连接设备。
- 抽象物模型: 网关内部统一抽象为标准事件结构 (设备标识、协议类型、端点、属性、数值、时间戳)。
- 云端通信: 基于 Wi-Fi 运行 MQTT 客户端，上报设备状态及告警，接收云端控制指令与规则更新。支持断网本地缓存与联网断点续传。

#### 4. 自动化双引擎设计
- 本地边缘引擎 (TOML): 
  - 运行在网关内部，基于触发-条件-动作 (TCA) 模型。
  - 解析 SD 卡中的 rules.toml，实现脱机离线状态下的局域网毫秒级自治联动。
- 云端高级引擎 (Python):
  - 运行在云端服务器，订阅 MQTT 数据流。
  - 负责跨网关协同、多源数据融合、历史时序分析及第三方 Webhook 接口联动。

#### 5. 软件工程与语言选型
- 架构分工:
  - 底层系统与通信: 采用 C 语言 (ESP-IDF)，调用官方底层库以保证 Wi-Fi、BLE 及 Zigbee 射频协议栈的稳定共存。
  - 边缘规则引擎: ~~核心规则解析与状态机计算可采用 Rust 编写为静态库，通过 C FFI 接口与 C 语言主工程链接，利用 Rust 类型安全特性解析 TOML 并执行匹配。~~
  - 人机界面任务: 建立低优先级后台异步任务驱动 OLED 刷新，避免阻塞关键射频时序。

---

### ~~2026-11-10 C <-> Rust FFI 胶水层设计与落地~~
~~- **组件架构**:~~

  - ~~将 Rust 边缘规则引擎封装为独立的 ESP-IDF 标准组件（`tiny-gate/components/rule_engine`）。~~
  - ~~在 CMake 中通过 `add_custom_command` 驱动 `cargo build --target riscv32imac-unknown-none-elf --release` 编译为 `librule_engine.a` 静态库直连主固件。~~
- ~~**内存安全与零跨界拷贝**:~~
  - ~~统一物模型采用 C ABI 兼容结构体：事件入参 `tg_event_t`、动作出参 `tg_action_t` 及通用数值标签联合体 `tg_value_t`。~~
  - ~~触发结果采用 C 函数指针回调机制 (`tg_action_callback_t`)，执行动作直接在调用者栈与上下文内派发，杜绝跨语言堆内存分配与泄漏风险。~~
- ~~**宿主/嵌入式双模支持**:~~
  - ~~硬件目标：`#![cfg_attr(not(test), no_std)]`，目标架构 `riscv32imac-unknown-none-elf`，启用 `panic = "abort"` 与 size 优化。~~
  - ~~单元测试：在 PC 宿主机直接运行 `cargo test --lib`，支持极速本地断言与 TCA 规则算法验证。~~

### 2026-11-12 以Rust为基座，esp-idf初始化C组件库

推翻先前设计，由于C是驱动层，而Rust是调用者，反而用cargo管理rust项目，用rust调用C函数更方便。因此推翻了先前C底座，rust作为组件的设计。

新的设计：rust为底座，以C为驱动层引出各设备的公开函数供rust层调用。

#### 落地实现与工程结构
- **Rust 顶层架构**:
  - 基于 `esp-idf-svc` (std 路线) + `embuild`，目标芯片 `esp32c6` (RISC-V 目标架构 `riscv32imac-esp-espidf`)。
  - 框架版本：升级至 `esp-idf-svc 0.53`，ESP-IDF 底层 SDK 全面切换至 `v6.1`，通过 `.cargo/config.toml` 配置 `ESP_IDF_VERSION = "v6.1"`。
  - 链接与运行时：通过 `.cargo/config.toml` 配置 `ldproxy` 链接器与 `build-std = ["std", "panic_abort"]`。
- **C 驱动组件下沉 (`components/c_drivers`)**:
  - 独立 ESP-IDF 标准组件结构，包含 `CMakeLists.txt`、`include/c_drivers.h` 与 `src/c_drivers.c`。
  - 模块化暴露硬件接口：电源掉电检测与保持电压采样（2026-11-21 起为超级电容方案）、OLED SSD1306 显示流、MicroSD 存储挂载、Zigbee 3.0 协调器与 BLE 接入能力。
  - 在 `Cargo.toml` 中配置 `package.metadata.esp-idf-sys.extra_components` 联动 ESP-IDF 构建体系，由 Rust `src/drivers/board/power.rs` 统一提供类型安全封装。

### 2026-11-13 网关、云端与客户端通信契约

- Rust 网关通过 MQTT/JSON 与云端通信；Web 与微信小程序通过 HTTPS/JSON 共用后端 API，WSS 提供状态通知。
- 接口、公共模型、命令生命周期、错误码及联调样例统一维护在 [contracts/README.md](contracts/README.md)。该目录是协作开发基线，不表示功能已经实现。
- 硬件/C 负责设备协议映射，Rust 负责网关消息与执行语义，云端负责 API、权限与客户端；跨边界修改共同确认并同步更新契约与样例。
- 首期通过两台 ESP32-S3 BLE 执行模拟器验证独立控制、实际反馈、超时和重复请求；当前扫描接口不能视为已完成 GATT 控制。

### 2026-11-20 屏幕只能由 UI 层更新

- **规则**：OLED 的所有更新必须经由 `crate::ui`（`Ui` 及其方法），任何业务代码不得直接持有或调用 `Display` 驱动。
- **落地**：
  - `Display` 从 `Hardware` 槽位中移除，由 `Ui::new()` 自行探测并独占整个固件周期；业务代码从类型上就拿不到屏幕驱动，旁路在编译期即被封死。
  - 旧的 `update_oled(display, ...)`（直写 `show_status`，会覆盖第 0 行）废弃，替换为 `update_status_page(ui, line1, line2)`：写状态对（第 2/4 行）、保留顶栏，同样隐藏探测降级的 `Option`。
  - `radio_bench` 不再从 `Hardware` 拆出 `Display` 自建 `Console`；`zigbee_demo` / `coexist::run` 的屏幕参数统一改为 `&mut Option<Ui>`。
- **理由**：顶栏（第 0 行）成为全局持久状态条（无线电图标 + 时间），只有 UI 层有权编排行布局；同时为后续"UI 任务 + 消息队列"的多任务演进预留唯一所有者。
- 显示屏生命周期约定：屏幕所有者（当前为 `main` 持有的 `Ui`）park 在主循环中即可停帧观察；释放所有权 = 干净关机（熄屏 + 释放 I2C）。禁止用 `forget` 保帧。

### 2026-11-21 M1 重构：网关产品主线取代演示分发

- **分层确认（C 底座 / Rust 业务）**：C 组件拥有全部底层读写；Rust 只做上层业务。`drivers/`、`radio/`、`platform/` 仅是 `tg_*` C 函数的薄封装；`gateway/` 不直接触碰硬件 API（identity 的 NVS 属业务数据持久化，走 esp-idf-svc 服务封装，是唯一显式例外）。全部验证 bench 移入 `demo` 分支（基线 c3bdc22）。
- **异步模型（embassy-on-esp-idf）**：`embassy-executor 0.10`（platform-std + executor-thread）+ `embassy-time 0.5`（官方 std driver，自起 tick 线程）+ `embassy-sync` 通道。纯 esp-hal/no_std 路线不可行——三射频共存绑定 ESP-IDF。双 executor：主 executor 跑无线电服务与定时器；UI 独立线程跑自己的 executor 并自降优先级，落实"低优先级人机界面任务"。
- **物模型与总线**：`gateway/model.rs` 按 contracts v1 定形（属性名 snake_case、值 bool/number/string、顶栏/状态行渲染约定）；`gateway/bus.rs` 单一静态 embassy 通道（32 深与 C 队列对齐），服务 `try_send`、UI `try_receive`。
- **连接身份**：`gateway/identity.rs` NVS 持久化 `boot_counter`、boot 级 `boot_id`、启动内递增 `seq`；`gateway_id` 占位 `tinygate-dev`，配网里程碑落地。
- **Zigbee 服务**：demo 的入网/发现/配置上报/在场流程迁入 `gateway/zigbee.rs`（60 s 入网窗、HA profile 端点匹配、15 s 在场判定，`gateway/presence.rs` 纯逻辑可宿主测试）；命令执行与 TX-confirm 验证留待 M2 复用 `radio/zigbee_check`。
- **Send 标记放宽**：`Display`/`Coordinator` 移除 `PhantomData<Rc<()>>` 的 !Send 断言——embassy 任务需跨 await 持有；单实例由 AtomicBool 守卫，串行契约由服务任务结构保证（executor 不迁移任务线程）。
- **critical-section 实现**：esp-idf crates 不注册 `critical-section` 符号；`src/app/runtime.rs` 以 FreeRTOS `vPortEnterCritical/vPortExitCritical` 提供全固件唯一实现，供 embassy-sync 总线使用。
- **构建修复前置**：`build.rs` 现转发 ESP-IDF configure 环境给 ninja，regen 失败降级为 warning（此前任何依赖变更都会 panic 并死锁构建）。
- **待上板验证**：std tick 线程与双 executor 定时器唤醒；ZB 图标随协调器 ready 点亮；入网窗内传感器上报 → 顶栏时钟秒级走动 + "PEER/TEMP" 状态行；复位后 `boot_counter` 递增（NVS 生效）。

### 2026-11-21 Rust 代码按产品能力重组

- **问题**：原 `src/` 仍按验证 bench 的形状划分——`drivers/`、`radio/`、`platform/` 三套平行的 C 外壳，加上根目录的 `hardware.rs`/`runtime.rs`；产品能力（接入、物模型、上云、规则）没有自己的落点。
- **新顶层**：`app/`（组合根与板级装配）、`drivers/`（全部 `tg_*` 薄封装 + 统一 Lease）、`gateway/`（产品领域与能力服务）、`ui/`（唯一屏幕写者）。详见 [docs/architecture.md](docs/architecture.md)。
- **依赖方向**：`main → app → gateway → drivers`；`ui → drivers`（仅 display）；`drivers` 不反向依赖业务。
- **产品落点**：Zigbee 服务迁至 `gateway/access/zigbee/`；物模型与事件拆为 `gateway/device.rs`、`gateway/event.rs`；后续 M2 命令、M3 上云、M4 规则分别落在 `gateway/command.rs`、`gateway/uplink/`、`gateway/rules/`，不在 `src/` 预建空模块。
- **UI 拆层**：`console/` 状态机不再依赖 `Display`，由 `ui/mod.rs` 作为后端适配；`icon.rs`、`macros.rs`、`view.rs` 分离；`Value::display` 从物模型移到 `ui/view.rs`，领域层不再知道 5x7 字体。
- **历史路径**：旧 `src/c_drivers.rs` 现为 `src/drivers/board/power.rs`；旧 `src/runtime.rs` 现为 `src/app/runtime.rs`。

### 2026-11-21 电源改为超级电容保持

- **决定**：取消 18650 锂电池与电池管理。电源路径改为超级电容保持，只提供主电源丢失后的短时保存窗口，不追求续航或持续运行。
- **产品行为**：检测到 mains loss → 立即停止射频/上云等非必要工作 → 在保持窗口内按优先级保存关键状态 → 停止业务并等待电源耗尽；恢复供电按冷启动处理，`boot_counter` 递增。
- **保存内容**：命令去重与结果、离线事件队列、日志尾、必要的设备/规则状态。保存顺序与实现由存储/命令模块提供，电源驱动不决定业务写什么。
- **不提供的接口**：电池电压、SoC、剩余续航、长时间 UPS 运行。`drivers/board/power.rs` 只暴露 `init`、`mains_lost`、`rail_voltage`；`rail_voltage` 仅用于估算保存窗口，不是电量。
- **窗口标定**：超级电容容量、掉电检测 GPIO/ADC 引脚与保存预算待硬件确认并实测；未标定前不得在代码或 UI 承诺具体毫秒数，也不得把“已发起保存”报告为“已保证落盘”。
- **落点**：`components/tg_power` 保留电源保持接口，等待板级检测与采样；Rust 门面为 `drivers/board/power.rs`；掉电编排在 `gateway/power.rs`，依赖 M3 的存储与命令日志。

### 2026-11-28 Rust / C 责任边界

- **背景**：2026-11-12 定下“Rust 底座、C 驱动”，2026-11-21 定下“C 拥有底层读写、Rust 只做上层业务”。但 `b47da04`、`3fdafd0` 由 C 侧直接改写 `src/gateway/`、`src/drivers/` 并在 `src/**` 内加入测试模块，分层只剩文档。自本条起按目录与提交人划定边界，不再靠默契。
- **归属**：
  - Rust 主线（kanaDE）：`src/**`、`Cargo.toml`、`Cargo.lock`、`build.rs`、`.cargo/**`、`rust-toolchain.toml`、`tests/zigbee-host/**`。`src/**` 内的所有 `#[cfg(test)]` 测试模块同属 Rust 主线。
  - C 组件（Reacher）：`components/**`，含 `include/` 公开 ABI 头、`src/` 实现、`CMakeLists.txt`、`Kconfig`、`idf_component.yml`；C 侧测试放 `components/<name>/test/**`（设备端 Unity）或 `tests/*_test.c`（主机 gcc）；对应驱动文档也由 C 侧维护。
  - 共享、需双方确认：`contracts/**`、`DESIGN.md`、`docs/architecture.md`、`partitions.csv`、`sdkconfig.defaults`、`components_esp32c6.lock`，以及 `Cargo.toml` 中 `extra_components` / `bindings_header` 这类跨边界构建接线。
- **硬规则**：
  1. C 侧不修改 `src/**` 下任何文件。为了测试 C 组件也不行：mock、fixture、临时打印、`#[cfg(test)]` 模块都不许塞进 `src/`。C 组件的测试落点见上。
  2. Rust 侧不直接改 `components/**` 的 C 实现。发现 C 侧缺陷走 issue 或 PR 评论，由 C 侧修复；Rust 侧只维护 `src/drivers/**` 里的 `extern "C"` 声明与类型安全门面。
  3. ABI 变更（`components/*/include/tg_*.h`、`components/platform_bindings.h`）必须成对交付：C 头文件变更与 Rust 门面适配放在同一变更集，或明确拆成“C ABI → Rust 适配”两个连续 PR。只改头导致主线编译失败不算完成，PR 描述必须写“ABI 变更”。
  4. 一个 PR 只动一侧目录。跨边界改动必须显式标注并请对方 review；禁止借 `feat(zigbee)` 之类 scope 顺带重排、格式化、改写对方目录。
  5. 判定标准是语言归属，不是目录名：Rust 代码（含 `tests/zigbee-host/**` 这类宿主测试夹具）只由 Rust 侧维护。C 侧可以运行该夹具、在 issue 里提出用例需求，但不能直接改；现有 Rust API 不可测时，由 Rust 侧先做可测性重构。
- **图标豁免（唯一例外）**：Rust 侧可以为新增或调整 OLED 图标，直接修改 `components/tg_display/src/display_canvas.c` 的 `icons[]` 半字形表与码位注释，并在同一 PR 更新 `src/ui/icon.rs` 的常量。范围仅限图标数据；不得顺手改 `glyph()`、字体表、`tg_canvas_*`、`display_canvas.h` 或其他 C 文件。需要扩大图标数组结构或调整码位规划时先与 C 侧确认。
  - 当前 `0x90..0xFF` 被 `tests/display_canvas_test.c` 约定为空白；扩展图标若越过 `0x8F`，由 C 侧同步调整该测试，Rust 侧不直接改 `tests/*_test.c`。
  - 除本豁免外，`components/**` 仍按第 2 条归 C 侧。
- **测试责任**：
  - C 驱动行为（SDK 调用、队列、绑定/上报、掉电采样）由 C 侧在 C 测试中证明，不能只靠 Rust 单测反推。
  - Rust 业务语义（去重、超时、状态机、UI 渲染）由 Rust 侧在 `src/**` 单测或 `tests/**` 宿主测试中证明；测试 mock 的是 `drivers` 门面，不是 C 源码。
  - 上板联调共同进行，验收记录分清各自负责项；“联调需要”不构成 `src/**` 的写权限。
- **执行**：`.github/CODEOWNERS` 对两侧目录分别要求对应 owner review，PR 模板加入边界检查项；GitHub 分支保护需开启 “Require review from Code Owners” 才会真正生效。发现越界提交时要求拆分，不合并混合提交。
- **历史处理**：`b47da04`、`3fdafd0` 已合并的混合改动不回滚；其中位于 `src/**` 的测试模块由 Rust 侧接手维护，后续修改按本条执行。若其中测试实际在验证 C ABI 语义，迁移到 `components/<name>/test/**` 或 `tests/*_test.c`，不留在 `src/`。



