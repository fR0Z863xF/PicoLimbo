# Action Bar 持久化显示实现说明

## 概述

实现了 action_bar 信息的持久化显示功能。action_bar 会在玩家登录时立即显示，然后每 3 秒自动重新发送，确保信息持续显示在玩家屏幕上。

## 实现细节

### 1. 修改的文件

#### `pico_limbo/src/server/client_data.rs`
- 添加了独立的 `action_bar_interval` 定时器（与 `keep_alive_interval` 分离）
- 新增 `enable_action_bar()` 方法：启用 action_bar 定时发送（每 3 秒，第一次立即触发）
- 新增 `action_bar_tick()` 方法：等待下一次 action_bar 发送时机
- 修改 `shutdown()` 方法：清理 action_bar 定时器

#### `pico_limbo/src/server/network.rs`
- 在 `read()` 函数的 `tokio::select!` 中添加了 `action_bar_tick()` 分支
- 新增 `send_action_bar()` 函数：根据协议版本发送对应的 action_bar 数据包
  - 1.17+ 使用 `SetActionBarTextPacket`
  - 1.11-1.16 使用 `LegacySetTitlePacket::action_bar()`
  - 1.8-1.10 使用 `LegacyChatMessagePacket::game_info()`
- 在 `process_packet()` 函数中，当玩家**刚进入** Play 状态时启用 action_bar 定时器
- 添加必要的导入：`ProtocolVersion`, `SetActionBarTextPacket`, `LegacySetTitlePacket`, `LegacyChatMessagePacket`

#### `pico_limbo/src/handlers/configuration.rs`
- 新增 `enable_action_bar_if_needed()` 公共函数：判断是否需要启用 action_bar
  - 检查协议版本 >= 1.8
  - 检查服务器配置中是否设置了 action_bar

#### `pico_limbo/src/handlers/mod.rs`
- 导出 `enable_action_bar_if_needed` 函数供其他模块使用

### 2. 工作流程

```
玩家连接 → 登录 → 进入 Play 状态
                        ↓
            在 send_play_packets 中发送第一次 action_bar
                        ↓
            检查是否配置了 action_bar
                        ↓
                   启用定时器
                        ↓
            立即触发第一次（重复显示，确保可见）
                        ↓
            之后每 3 秒发送 action_bar 数据包
```

### 3. 定时器行为

使用 `tokio::time::interval`，其特性是：
- **第一次 tick 立即完成**：这意味着启用定时器后，事件循环会立即触发一次 action_bar 发送
- **后续 tick 按周期触发**：每 3 秒触发一次

这样设计的好处：
1. 玩家登录时在 `send_play_packets` 中发送第一次（立即显示）
2. 启用定时器后，第一次 tick 立即完成，再次发送（确保显示）
3. 之后每 3 秒持续发送（保持显示）

### 4. 协议版本兼容性

| Minecraft 版本 | 使用的数据包类型 |
|---------------|----------------|
| 1.17+ | `SetActionBarTextPacket` |
| 1.11 - 1.16 | `LegacySetTitlePacket` (action_bar 模式) |
| 1.8 - 1.10 | `LegacyChatMessagePacket` (game_info 模式) |
| < 1.8 | 不支持 action_bar |

### 5. 配置示例

在 `server.toml` 中配置 action_bar 内容：

```toml
# 使用 MiniMessage 格式
action_bar = "<green>Welcome to <bold>PicoLimbo</bold>!</green>"

# 或使用传统颜色代码
action_bar = "§aWelcome to PicoLimbo!"

# 留空则不显示
action_bar = ""
```

### 6. 性能考虑

- 使用独立的定时器，不影响 keep_alive 机制
- 只在配置了 action_bar 且协议版本支持时才启用
- 使用 `tokio::select!` 实现非阻塞的多路复用
- 3 秒的发送间隔平衡了显示效果和网络开销
- 第一次立即触发，确保玩家进入游戏后立即看到 action_bar

## Bug 修复记录

### 问题：第一次显示后需要等待才会再次显示

**原因**：
- 在 `send_play_packets` 中发送了第一次 action_bar
- 然后启用定时器，但定时器的第一次 tick 需要等待 3 秒
- 导致玩家看到第一次显示后，要等 3 秒才会再次显示

**解决方案**：
- 使用 `tokio::time::interval` 而不是 `interval_at`
- `interval` 的第一次 tick 会立即完成
- 这样在启用定时器后，事件循环会立即触发一次发送
- 实现了：登录时显示 → 立即再次显示（确保可见）→ 每 3 秒持续显示

## 测试建议

1. 启动服务器，配置 action_bar 内容
2. 使用不同版本的 Minecraft 客户端连接（1.8, 1.11, 1.17+）
3. 观察 action_bar 是否在登录后立即显示
4. 验证 action_bar 是否持续显示（每 3 秒刷新）
5. 验证在玩家移动、执行命令等操作时 action_bar 仍然可见

## 注意事项

- action_bar 会在玩家进入 Play 状态后自动启用
- 如果配置文件中 `action_bar` 为空字符串，则不会启用定时发送
- 1.8 以下版本不支持 action_bar 功能
- 第一次显示会有轻微的重复（登录时 + 定时器第一次），这是为了确保显示效果
