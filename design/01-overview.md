# 01 · 项目概览

## 1.1 定位

μAgent 是一个**设备端 agent 编排内核**。目标部署形态:
- iOS / iPadOS / macOS app 内嵌库
- Android app 内嵌库
- Raspberry Pi / OpenWrt / NAS 等 Linux SBC daemon

## 1.2 做什么、不做什么

**做:**
- Agent 主循环(think → act → observe 多轮)
- 工具调用(native tool_use + 3B 小模型的 ReAct-fallback)
- Skill 打包(可复用能力单元,带 prompt 提示 + 工具 + 状态)
- MCP 客户端(双重 lazy,不吃 context)
- 错误分层 + 自愈(stall 检测、重试预算、降级)
- 会话持久化 + 审计事件流
- 资源预算(tokens / turns / wall / 背景时间)
- 平台能力门禁(编译 feature + runtime profile + Adapter 层 probe)

**不做:**
- LLM 推理引擎(交给 llama.cpp / MLX / ExecuTorch / Apple FM / AICore)
- UI(由 host app 提供)
- 模型训练 / 微调
- 自然语言理解之外的"智能"(比如 vision pipeline 等属于工具范畴)

## 1.3 与同类的差异

| 维度 | pi-mono | Cactus | Apple FM / AICore | fullmoon / PocketPal | **μAgent** |
|---|---|---|---|---|---|
| 开源 | ✅ | ❌ | ❌ | ✅ | ✅ |
| 跨平台 | 桌面为主 | ✅ | ❌ 锁单一生态 | ✅ | ✅ |
| Agent loop | ✅ | ✅ | ✅ | ❌ chat-only | ✅ |
| Tool calling | ✅ | ✅ | ✅ | ❌ | ✅ |
| MCP | ✅(前置) | ? | ❌ | ❌ | ✅(lazy) |
| 错误分层 | ❌ 吞错 | ? | ✅ | n/a | ✅(核心卖点) |
| 平台沙盒感知 | ❌ | ✅ | ✅ 原生 | n/a | ✅ Adapter 层 |
| 系统 LLM 优先 | ❌ | ✅ | 原生 | ❌ | ✅ FM/AICore 作 backend |

**独立价值**:填补"开源 + 跨平台 + 带错误处理 + 能接入系统 LLM"这个空档。

## 1.4 目标用户

- iOS/Android 开发者想给 app 加 on-device agent(例如生产力工具、笔记、IDE 移动端、家居控制 app)。
- SBC 开发者想给 Pi / 路由器装 agent daemon(例如 NAS 助手、智能家居 controller)。
- 模型厂商想出一套 reference agent runtime 配合他们的小模型。

## 1.5 非目标

- 不追求"全球最快的 loop"——错误正确性优先于吞吐。
- 不追求"自动发现 MCP 工具并全量装载"——明确拒绝 pi 7-9% context overhead。
- 不做 sub-agent / 并行 agent 编排(第一版)。
