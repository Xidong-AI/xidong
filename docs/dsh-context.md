# 智能体系统分层上下文与透明性参考设计 v0.1

## 0. 文档目的

为智能体系统设计一套**可审计、可扩展、缓存友好**的上下文管理机制。核心目标：

- 模型看到什么、用户看到什么、系统记录什么，三者可对齐、可追溯。
- 上下文不是静态拼接，而是按轮次动态装配。
- 动态变更以追加方式进入历史，尽量不破坏 KV Cache 前缀。
- 每条注入内容都有持久化来源标识，并能在模型可见文本中体现来源。

---

## 1. 设计原则

| 原则 | 说明 |
|---|---|
| 仅追加 | 会话日志只追加，不重写历史事件。 |
| 唯一真源 | 事件日志是权威来源，模型消息和 UI 视图都从日志派生。 |
| 来源持久化 | 每条消息带 `source` 元数据，标识生产者、角色、路径、哈希。 |
| 呈现层信封 | 生产者负责把来源信息写入模型可见文本，如 `<system-reminder>`。 |
| 缓存友好 | 易变内容放历史尾部，稳定内容放系统提示词开头。 |
| 适配器隔离 | 不同 LLM Provider 的能力差异由适配器吸收。 |
| 透明可审计 | UI 能展示完整系统提示词、事件日志和来源徽章。 |

---

## 2. 总体架构

```
┌─────────────────────────────────────────────┐
│                  UI 呈现层                   │
│  对话视图 / 轨迹面板 / 来源徽章 / 日志下载    │
└──────────────────▲──────────────────────────┘
                   │ 投影
┌──────────────────┴──────────────────────────┐
│              上下文组装层                     │
│  SystemPrompt.assemble()                    │
│  sections + contexts + messages + tools     │
└──────────────────▲──────────────────────────┘
                   │ 派生
┌──────────────────┴──────────────────────────┐
│           会话事件日志（唯一真源）             │
│  user/message, assistant/message,           │
│  tool/call, tool/result, system/message,    │
│  context/injection ...                      │
└──────────────────▲──────────────────────────┘
                   │ 写入
┌──────────────────┴──────────────────────────┐
│        插件 / 工具 / 运行时 / 用户            │
└─────────────────────────────────────────────┘
```

---

## 3. 事件日志：唯一真源

### 3.1 事件结构

每条事件建议包含：

```ts
interface SessionEvent {
  seq: number;              // 全局递增序号
  time: number;             // 时间戳
  type: string;             // user/message, assistant/message, tool/result ...
  data: unknown;            // 事件载荷
  source: SourceRef;        // 来源标识，持久化
  surfaceOp?: SurfaceOp;    // 呈现操作：inject, recall, replace ...
  provenance?: Provenance;  // 人类可读的溯源信息
}
```

### 3.2 `source` 来源标识

`source` 是让系统识别“谁提供了这条内容”的权威元数据：

```ts
interface SourceRef {
  kind: "user" | "plugin" | "agent" | "tool" | "system";
  plugin?: string;          // 插件 ID，如 agent-instructions
  role?: "inject" | "recall" | "replace" | "baseline";
  label?: string;           // 人类可读标签，如 AGENTS.md
  path?: string;            // 文件路径
  hash?: string;            // 内容哈希，用于去重和对账
}
```

关键点：**`source.kind` 不直接渲染进模型文本，但它决定消息如何被投影和格式化**。

---

## 4. 上下文组装

每个模型调用步骤前，执行一次组装：

```ts
interface ModelRequest {
  system: string;           // 渲染后的系统提示词
  messages: Message[];      // 历史消息 + 注入消息
  tools: ToolSchema[];      // 工具定义
}
```

组装来源四类：

| 组件 | 来源 | 特点 |
|---|---|---|
| sections | 系统提示词插件 | 有序段落，稳定，放 `messages[0]` |
| contexts | 运行时状态插件 | 易变，以 user-role 快照注入历史尾部 |
| messages | 会话日志派生 | 只追加，不重写 |
| tools | 工具注册表 | 独立字段，不占 message token |

**组装顺序建议**：
1. 系统规则段落按 `order` 升序拼接。
2. 动态状态只在变化时生成新事件，追加到历史尾部。
3. 历史消息由 `deriveMessages()` 从日志投影。
4. 工具 schema 独立发送。

---

## 5. 来源识别与格式化

### 5.1 模型可见信封

即使注入内容以 `user` 角色发送，也必须让模型识别其真实来源。推荐统一信封：

```text
<system-reminder source="agent-instructions" path="AGENTS.md" hash="abc123">
以下内容由系统注入，不是用户输入：
- 项目使用 TypeScript。
- 提交前运行 pnpm test。
</system-reminder>
```

要点：
- 标签名统一，如 `<system-reminder>`。
- 属性包含 `source`、`path`、`hash`。
- 正文开头明确“这是系统注入，不是用户输入”。
- 对模型不解析属性的情况，正文说明仍然可读。

### 5.2 优先级与冲突

在 `source` 中增加优先级语义，或在渲染文本中注明：

| 来源 | 建议优先级 |
|---|---|
| 系统提示词 | 100 |
| AGENTS.md / 工作区指令 | 80 |
| 运行时上下文 | 60 |
| 真实用户消息 | 50 |
| 工具结果 | 40 |

冲突时，高优先级覆盖低优先级。模型可见文本可写：“优先级：高，覆盖此前冲突指令。”

### 5.3 去重与哈希

- 相同 `hash` 的内容不重复注入。
- 文件变更时，追加“替换通知”，不重写旧事件。
- 恢复会话时，对账基线哈希；若变化影响缓存，折叠新基线或追加通知。

---

## 6. 缓存友好策略

### 6.1 追加式更新

- 动态状态变化：追加一条 `user/message`，内容为 `<system-reminder>当前工作目录已切换为 ...</system-reminder>`。
- `AGENTS.md` 变化：追加新指令或替换通知，不修改旧消息。
- 结果：系统提示词和历史前缀字节不变，KV Cache 可继续命中，仅尾部新增 token 需计算。

### 6.2 历史内 System Prompt 替换

当系统提示词自身需要更新时：

| Provider 能力 | 做法 | 缓存影响 |
|---|---|---|
| 原生支持历史 `system` | 追加一条 `system/message` 到历史尾部 | 前缀稳定，仅尾部新增 |
| 仅允许开头 `system`（如 OpenAI Chat Completions） | 降级：追加 `user/message`，内容为 `<system-reminder>` 系统提醒 | 旧 `system` 保持开头，前缀可命中；新指令在尾部 |
| 不支持任何变更 | 重写 `messages[0]` | 从第一个 token 起缓存全量失效 |

### 6.3 Provider 适配差异

| 能力 | OpenAI Chat Completions | DeepSeek 原生 |
|---|---|---|
| `system` 位置 | 仅 `messages[0]` | 可出现在历史任意位置 |
| 历史内 `system` 替换 | 不支持，降级为 `user` 消息 | 支持，追加 `system/message` |
| 角色语义 | 注入内容伪装为用户 | 保留系统角色 |
| 缓存优化 | 前缀可命中，但角色语义弱 | 前缀可命中，角色语义强 |

---

## 7. 用户与 LLM 的分别呈现

### 呈现给用户

- 对话视图：只显示用户消息、助手回复、工具状态。
- 来源徽章：对注入内容显示“来自 agent-instructions / 运行时上下文”。
- 轨迹面板：可查看完整系统提示词、组装结果、原始事件日志。
- 日志下载：提供 `session.jsonl`，包含所有事件和 `source` 元数据。

### 呈现给 LLM

- 结构化消息列表：`system` + `messages` + `tools`。
- 系统提示词已渲染，动态上下文已嵌入。
- 注入消息以 `<system-reminder>` 等信封标明来源。
- 模型看到的一切都写入日志，确保“模型可见即可记录”。

---

## 8. 实现清单

| 模块 | 职责 |
|---|---|
| EventLog | 仅追加写入、读取、重放 |
| SourceRegistry | 注册来源类型，生成 `source` 元数据 |
| ContextAssembler | 收集 sections、contexts、messages、tools |
| DeriveMessages | 从日志投影模型消息，处理 Provider 能力 |
| EnvelopeFormatter | 生成 `<system-reminder>` 等模型可见信封 |
| ChangeWatcher | 监听文件、目录、运行时状态变化 |
| ProviderAdapter | 吸收 OpenAI / DeepSeek 等 API 差异 |
| ProvenanceView | 为 UI 生成来源徽章和溯源视图 |

---

## 9. 最小可行路线图

1. 定义事件 schema，实现仅追加日志。
2. 实现 `deriveMessages()`，支持系统提示词 + 历史 + 工具。
3. 为注入消息增加 `source` 元数据。
4. 实现 `<system-reminder>` 格式化。
5. 加入动态状态变更监听，只在变化时追加。
6. 实现 Provider 适配器，区分 OpenAI 与原生支持历史 `system` 的模型。
7. 在 UI 中展示来源徽章和完整日志。
8. 测试缓存命中率、来源可识别性、指令冲突覆盖。

---

## 10. 反模式

- ❌ 每轮重复注入相同动态状态，导致尾部 token 浪费。
- ❌ 直接重写历史消息，破坏缓存前缀和审计性。
- ❌ 注入内容不带来源标记，模型误认为是用户意图。
- ❌ 把插件注入伪装成真实用户消息且不加说明。
- ❌ 依赖单一 `system` 消息承载所有内容，导致易变内容污染稳定前缀。
- ❌ 日志与模型可见内容两套口径，无法对账。
