## 改动范围

- [ ] 只改 Rust 侧（`src/**`、Cargo/构建、`tests/zigbee-host/**`）
- [ ] 只改 C 组件（`components/**`、C 测试、驱动文档）
- [ ] 跨边界改动（必须说明 ABI 影响，并 @ 对方 review）

## 边界检查

- [ ] 没有在 `src/**` 里添加 C 组件相关的 mock、fixture 或 `#[cfg(test)]` 测试
- [ ] 没有顺手格式化、重排或改写对方目录
- [ ] 若改了 `tg_*.h` / `platform_bindings.h`：已同步 Rust `extern "C"` 与 `src/drivers/**` 门面，并注明“ABI 变更”
- [ ] 若改了 `components/tg_display/src/display_canvas.c`：仅动 `icons[]` 图标半字形与码位注释（图标豁免），并同步 `src/ui/icon.rs`

## 验证

- Rust：`cargo build` / `cargo test`（宿主测试命令）
- C：`components/<name>/test` 或 `tests/*_test.c` 的构建与运行结果
