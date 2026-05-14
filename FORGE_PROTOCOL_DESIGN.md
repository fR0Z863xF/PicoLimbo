# Forge 协议接入 · 端到端技术方案

> **范围**：FML2 (Mc 1.13–1.20.1) + FML3 (Mc 1.20.2+ Forge/NeoForge)
> **策略**：Login/Configuration 阶段使用 **Snapshot 重放**，Status 阶段使用 **60 秒 TTL 实时透传缓存**
> **目标**：让 Forge/NeoForge 客户端在服务器列表显示绿✓，并且能正常进入 PicoLimbo 世界，后续转送到目标 server 后 mod 功能正常
> **非目标**：不保证客户端在 limbo 世界里能使用 mod 功能；FML1 (1.7-1.12) 暂不支持

---

## 1. Forge 协议事实摘要

### 1.1 客户端 hostname 后缀（Handshake state）

Forge 客户端在 handshake 包的 `hostname` 字段末尾追加 NUL 分隔的标记：

| 后缀 | 含义 | 适用版本 |
|------|------|----------|
| `\0FML\0`  | FML1（vanilla 风格 PluginChannel） | 1.7-1.12.2 |
| `\0FML2\0` | FML2 协商（Login Plugin Request） | 1.13-1.20.1 |
| `\0FML3\0` | FML3 协商（Configuration Plugin Message） | 1.20.2+ Forge/NeoForge |

PicoLimbo 当前仅在 `handlers/handshake.rs::begin_login` 把 hostname 交给 BungeeCord 检测使用。我们需要在调用 `check_bungee_cord` **之前**先判定 Forge 后缀（因为 BC 走的是 `\0` 分隔的另一种格式，必须先 strip Forge 后缀再交给 BC 解析）。

### 1.2 FML2 协商流程（Mc 1.13 – 1.20.1，Login state）

```
client                                    limbo
  │── Handshake (hostname: ...\0FML2\0)──▶│
  │── LoginStart ──────────────────────▶ │
  │                                       │ (replay 第 1 个包)
  │◀── LoginPluginRequest #1            ──│  channel=fml:loginwrapper
  │                                       │  payload=<embedded ServerHello>
  │── LoginPluginResponse #1 ───────────▶│
  │                                       │ (按 client 响应推进状态机, 取下一录制包)
  │◀── LoginPluginRequest #2 (ModList) ──│
  │── LoginPluginResponse #2 (ModList) ─▶│
  │◀── LoginPluginRequest #3 (RegistryData/Acknowledge) ──│
  │── LoginPluginResponse #3 ───────────▶│
  │                                       │ (录制中下一步是 server 发 LoginSuccess —— 拦截、改用 limbo 自己的)
  │◀── LoginSuccess (limbo 自己的) ──────│
  │── (configuration / play 由 limbo 接管) │
```

关键点：
- **LoginPluginRequest** = clientbound packet ID `0x04`（也就是 PicoLimbo 中 `CustomQueryPacket`，已实现编码）
- **LoginPluginResponse** = serverbound packet ID `0x02`（已实现 `CustomQueryAnswerPacket` 解码）
- channel 通常是 `fml:loginwrapper`（外层包装）+ 内嵌的 `fml:handshake` payload
- `message_id` 是 VarInt，每条 request 的 id 由 server 决定，client 必须用同一 id 回应
- **重放策略**：录制 server 发出的 `(channel, payload)` 序列，重放时 message_id 用 limbo 即时生成（client 看到什么 id 就回什么 id），用一张 `pending_id → snapshot_step` 的映射表追踪进度

### 1.3 FML3 协商流程（Mc 1.20.2+，Configuration state）

1.20.2 引入了 Configuration state，FML3 把握手挪到了这里，使用 **clientbound `0x01` Configuration Plugin Message** 和 **serverbound `0x02` Serverbound Plugin Message**（`channel + data`，无 message_id），channel 仍为 `fml:handshake`。

```
client                                    limbo
  │── Handshake (...\0FML3\0) ─────────▶│
  │── LoginStart ──────────────────────▶│
  │◀── LoginSuccess (limbo 直接发出) ──│
  │── LoginAcknowledged ──────────────▶│  (进入 Configuration state)
  │◀── ConfigPluginMessage(fml:handshake, ServerModList) ─│
  │── ConfigPluginMessage(fml:handshake, ClientModList) ─▶│
  │◀── ConfigPluginMessage(fml:handshake, RegistryData)  ─│
  │── ConfigPluginMessage(fml:handshake, Ack) ──────────▶│
  │◀── ConfigPluginMessage(fml:handshake, ConfigData) ───│
  │── ConfigPluginMessage(fml:handshake, Ack) ──────────▶│
  │◀── (limbo 转入 RegistryData + FinishConfiguration) ──│
```

关键点：
- 因为 configuration plugin message 没有 message_id，重放是**纯顺序**：每收到客户端一条 fml:handshake 响应，就推进一步，发出录制中的下一条 server payload
- 必须在 PicoLimbo 现有的 configuration state 处理流程**之前**插入 Forge 协商，等握手完成后再继续 PicoLimbo 标准的 RegistryData/UpdateTags/FinishConfiguration

### 1.4 Status 阶段的 forgeData

Forge 客户端 ping 服务器时，看到 status JSON 里**有 `forgeData` 字段**才会显示绿✓ + 可点入。我们的策略：

```
client → limbo: StatusRequest
limbo: 检查 forge_status_cache（TTL 60s）
  ├─ 命中且未过期: 直接合并到 limbo 的 status JSON 返回
  └─ 未命中: 异步连上游 forge server 拉一次完整 status JSON，提取 forgeData 字段，缓存 + 返回
```

`forgeData` 结构示例（NeoForge）：
```json
{
  "forgeData": {
    "channels": [{"res": "fml:handshake", "version": "1.2.3.4", "required": true}, ...],
    "mods":     [{"modid": "minecraft", "modmarker": "1.20.4"}, ...],
    "fmlNetworkVersion": 3,
    "d": "<gzip+base64 详细 mod 列表>"
  }
}
```

我们**不解析、不重组**，整体 base64 透传缓存。

---

## 2. 模块拆分（颗粒度 → 文件级）

### 2.1 新增文件

```
crates/minecraft_packets/src/handshaking/handshake_packet.rs   # +ForgeKind 解析
pico_limbo/src/configuration/forge.rs                          # [forge] 配置块
pico_limbo/src/forge/
    ├── mod.rs                  # 模块导出
    ├── forge_kind.rs           # enum ForgeKind { None, Fml2, Fml3 }
    ├── snapshot.rs             # SnapshotEntry / Snapshot { handshake_steps, status_forge_data }
    ├── snapshot_io.rs          # 序列化/反序列化（postcard）+ 文件 IO
    ├── upstream_client.rs      # 复用 PacketStream 作为 outbound MC client
    ├── recorder.rs             # 启动时连上游录制 login/config 阶段握手
    ├── replay.rs               # 运行时重放状态机
    └── status_proxy.rs         # Status 阶段拉取 forgeData + TTL 缓存
pico_limbo/src/server_state/forge_session.rs                   # 单连接的 Forge 会话状态
pico_limbo/src/handlers/configuration/                         # 当前空目录 → 实现 config plugin msg handler
    ├── mod.rs
    └── plugin_message.rs
```

### 2.2 修改文件

```
pico_limbo/Cargo.toml                                          # +postcard, +tokio TcpStream 已有
pico_limbo/src/lib.rs / main.rs                                # 启动时调用 recorder
pico_limbo/src/configuration/config.rs                         # 接入 ForgeConfig
pico_limbo/src/configuration/mod.rs                            # +pub mod forge
pico_limbo/src/server_state/mod.rs                             # 持有 Snapshot + StatusCache 句柄
pico_limbo/src/server/client_state.rs                          # +forge_kind, +forge_session
pico_limbo/src/handlers/handshake.rs                           # 解析 ForgeKind 写入 client_state
pico_limbo/src/handlers/login/login_start.rs                   # FML2 路径：启动握手序列
pico_limbo/src/handlers/login/custom_query_answer.rs           # 按 client_state.forge_session 分流
pico_limbo/src/handlers/login/login_acknowledged.rs            # FML3：进入 configuration 后 server 主动推第一帧
pico_limbo/src/handlers/configuration.rs                       # FML3：拦截 plugin message 推进握手
pico_limbo/src/handlers/status/status_request.rs               # 注入 forgeData
pico_limbo/src/server/packet_registry.rs                       # +ServerBoundPluginMessage(config) +ClientBoundPluginMessage(config)
crates/minecraft_packets/src/configuration/                    # +configuration_serverbound_plugin_message_packet
```

---

## 3. 关键数据结构

### 3.1 `ForgeKind`

```rust
// crates/minecraft_packets/src/handshaking/handshake_packet.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    None,
    Fml2,   // \0FML2\0  (1.13-1.20.1)
    Fml3,   // \0FML3\0  (1.20.2+)
}

impl HandshakePacket {
    /// Detects Forge marker and returns the cleaned hostname (Forge marker stripped)
    /// alongside the detected kind. Cleaned hostname is used for downstream BungeeCord
    /// detection, so we never break BC + Forge stacking.
    pub fn detect_forge(&self) -> (ForgeKind, String) { ... }
}
```

### 3.2 `Snapshot`

```rust
// pico_limbo/src/forge/snapshot.rs
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub upstream_addr: String,
    pub fml2: Option<Fml2Snapshot>,
    pub fml3: Option<Fml3Snapshot>,
    pub status_forge_data: Option<serde_json::Value>, // 启动时一并抓的 forgeData
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Fml2Snapshot {
    /// 服务器在 login plugin request 中发送的序列。每个条目是录制时按 message_id
    /// 升序排列。重放时按客户端响应顺序逐条触发 next-step。
    pub steps: Vec<Fml2Step>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Fml2Step {
    pub channel: String,    // 通常 "fml:loginwrapper"
    pub payload: Vec<u8>,   // 不含 message_id 的纯 data
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Fml3Snapshot {
    pub steps: Vec<Fml3Step>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Fml3Step {
    pub channel: String,    // "fml:handshake"
    pub payload: Vec<u8>,
}
```

### 3.3 `ForgeSession`（运行时单连接状态）

```rust
// pico_limbo/src/server_state/forge_session.rs
pub struct ForgeSession {
    pub kind: ForgeKind,
    /// 已发出但未收到响应的 message_id → step idx
    pub pending: HashMap<i32, usize>,
    /// 下一个待发送的 step idx
    pub next_step: usize,
    /// 当前会话用的 message_id 自增（FML2 only）
    pub next_message_id: i32,
}
```

### 3.4 `ForgeConfig`

```toml
# config.toml
[forge]
enabled = false                          # 总开关
upstream = "127.0.0.1:25566"            # bootstrap forge server 地址
snapshot_path = "./forge_snapshot.bin"  # 重放快照文件
record_on_start = true                  # 启动时自动连上游录制（false 则只读盘）
status_cache_ttl_secs = 60              # status forgeData 缓存 TTL
status_request_timeout_ms = 3000        # 上游 status 请求超时
record_timeout_ms = 10000               # 录制握手超时
```

---

## 4. 状态机细节

### 4.1 FML2 录制流程（recorder.rs）

```
1. open TcpStream to upstream
2. send Handshake { protocol = limbo's max supported, hostname = "limbo\0FML2\0", port, next=2 }
3. send LoginStart { username = "PicoLimboRecorder", uuid = nil }
4. loop:
     read packet (PacketStream uncompressed)
     match packet_id:
       0x03 SetCompression  -> stream.set_compression(threshold, default level)
       0x04 LoginPluginReq  -> snapshot.fml2.steps.push({channel, payload}); 同时按"假装已安装所有 mod"的策略生成 Response 回给上游推进协商
       0x02 LoginSuccess    -> 录制结束（不存这个包），关闭连接，写盘
       0x00 Disconnect      -> 录制失败，记 warn 日志，limbo 仍可启动但 forge 不可用
       其他                 -> 直接终止录制并报错
```

**"假装已安装所有 mod" 的客户端响应** 是录制器最棘手的部分。用以下兜底策略：
- 对 `fml:handshake` 内的 `ServerHello` → 回 `ClientHello` (协议版本镜像)
- 对 `ModList` → 回客户端 `ModList { mods: [], channels: [], registries: [] }`（"我啥都没装"）。这会让上游 forge server 把"完整 mod 列表"作为 RegistryData 推过来——这才是我们要录制的核心
- 对 `RegistryData` 推送 → 回 `Ack { phase = WaitingServerData }`
- 对 `Complete` → 回 `Ack { phase = Complete }`

**注意**：录制器自身的回包不会进 snapshot——snapshot 只存 server→client 方向的包。运行时 limbo 收到客户端真实响应时按相同策略推进（见 4.2）。

### 4.2 FML2 重放流程（replay.rs，运行时）

```
trigger: handshake 检测 ForgeKind::Fml2 + login_start handler 调用
1. session.next_step = 0; session.next_message_id = 1
2. send_next_step():
     step = snapshot.fml2.steps[session.next_step]
     id = session.next_message_id; session.next_message_id += 1
     queue CustomQuery { message_id: id, channel: step.channel, data: step.payload }
     session.pending.insert(id, session.next_step)
     session.next_step += 1
3. on CustomQueryAnswer { message_id, is_present, data }:
     若 message_id 不在 pending → 透传给标准 velocity 路径（兼容现有逻辑）
     否则 pending.remove(message_id)
     if session.next_step < snapshot.fml2.steps.len():
         send_next_step()
     else:
         调用 fire_login_success(...)（限 limbo 自己的 GameProfile）
```

**幂等保证**：客户端可能因网络重传发重复 response → 用 pending 表去重，未在 pending 的 id 直接忽略。

### 4.3 FML3 重放流程（在 configuration state）

```
trigger: client 发出 LoginAcknowledged 进入 Configuration state
1. session.next_step = 0
2. send_next_step():
     step = snapshot.fml3.steps[session.next_step]
     queue ConfigClientBoundPluginMessage { channel: step.channel, data: step.payload }
     session.next_step += 1
3. on serverbound ConfigPluginMessage { channel, data }:
     if channel == "fml:handshake":
         if session.next_step < snapshot.fml3.steps.len():
             send_next_step()
         else:
             // 握手完成，转入 limbo 标准 configuration: RegistryData → UpdateTags → FinishConfiguration
             session.kind = None; // 清除标记
             enter_standard_configuration_flow()
     else:
         // 非 forge channel，按 PicoLimbo 既有逻辑处理（minecraft:brand 等）
```

### 4.4 Status 透传（status_proxy.rs）

```
全局 OnceCell<Mutex<StatusCache>>
StatusCache { value: Option<serde_json::Value>, fetched_at: Instant }

on StatusRequest:
  cache = global.lock()
  if cache.value.is_some() && cache.fetched_at.elapsed() < ttl:
      forge_data = cache.value.clone()
  else:
      drop(cache.lock)
      tokio::spawn(refresh) (非阻塞 first time 走 await，避免连接竞态)
      forge_data = fetch_upstream_status_blocking(timeout)
      // 存进缓存

  build pico status json
  if let Some(fd) = forge_data:
      json["forgeData"] = fd
  return
```

---

## 5. 错误处理与降级

| 场景 | 行为 |
|------|------|
| `forge.enabled=false` | 完全跳过，行为等于今天 |
| 启动时录制失败（上游不可达/超时） | 打 warn，limbo 正常启动，forge 客户端仍能 ping 但无 ✓，连接进来后被 disconnect 给出友好提示 |
| 启动时录制成功但运行时 status 拉取失败 | 用 snapshot 中保存的 status_forge_data fallback；都无则 status 不带 forgeData |
| 客户端响应未在 pending 表 | 兼容 velocity，让 velocity 流程处理；都不命中则忽略 |
| Snapshot 文件存在但反序列化失败 | warn + 删除文件 + 触发 record_on_start 行为 |
| FML 客户端但 snapshot 缺对应 kind | 用 LoginDisconnect 友好提示"Forge {kind} not supported by this limbo, missing snapshot" |

---

## 6. 控制台命令

`/forge refresh` —— 由现有 `pico_limbo/src/server_state/server_commands.rs` 体系扩展。重新连上游录制 + 写盘 + 热替换内存中的 Snapshot（`Arc<RwLock<Snapshot>>`）。

---

## 7. 测试计划

```
1. 单元测试
   - ForgeKind::detect_forge：覆盖 none / FML / FML2 / FML3 / 双后缀（FML+BC）
   - Snapshot 序列化/反序列化 round-trip
   - StatusCache TTL 行为
   - Replay 状态机：mock client response 序列，断言 batch 输出顺序与 LoginSuccess 落点

2. 集成测试（feature-gated, 默认 ignore）
   - 启动一个本地 mock forge upstream（用 tokio listener 模拟）
   - 让 limbo 录制 → 验证 snapshot 文件
   - 用真实 vanilla mc client 模拟器跑 ping，断言 forgeData 出现
   - 用录制的客户端会话回放，断言能完成握手到 play state

3. 手工验收
   - 真实 Forge 1.20.1 客户端 + 真实 Forge server bootstrap → limbo 显示 ✓ → 加入 limbo
   - 真实 NeoForge 1.21 客户端 + NeoForge server bootstrap → 同上
```

---

## 8. 文档

- `docs/config/forge.md` —— [forge] 配置说明、bootstrap server 准备指南（必须 online-mode=false、关闭 forwarding）
- `docs/tutorials/forge.md` —— 端到端搭建教程
- 在 `README.md` "Features" 加一行 Forge 支持说明
- `CHANGELOG.md` 添加条目

---

## 9. 实现顺序（避免大爆炸 PR，每步可编译可测）

```
Step 1  ForgeKind 类型 + handshake.rs 解析（含单测）
Step 2  ForgeConfig + Config 接入（含 default 测试）
Step 3  Snapshot 数据结构 + snapshot_io（含 round-trip 测试）
Step 4  upstream_client（薄封装：连接 + PacketStream 复用）
Step 5  recorder（启动时调用，先空实现可解析 SetCompression+Disconnect）
Step 6  status_proxy + status_request 注入 forgeData（不依赖 recorder 也能用 snapshot）
Step 7  packet_registry 加 Configuration plugin message 双向 + minecraft_packets 配套
Step 8  ForgeSession + replay (FML2)
Step 9  replay (FML3) + configuration handler 接入
Step 10 /forge refresh 命令 + 文档
Step 11 端到端测试 + 修 bug
```
