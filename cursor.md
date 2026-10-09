# Cursor BYOK 服务端架构

## 目录与职责

以下为执行路径涉及的主要模块；不列易失真的估算行数。测试在 `server/tests/` 与各模块的 `#[cfg(test)]` 中，生成协议独立于业务代码。

```text
protocols/cursor/                  # 从真实 Cursor 客户端提取的六个协议包
support/
├── cursor-protocol-extractor/     # 协议扫描、作用域解析与 Go 消息生成
└── cursor-capture/                # Connect 流量解码与调试
server/
├── build.rs                      # Rust protobuf 生成及手写子集断言
├── prompt/cursor/                # 模式、工具及运行提示资产
├── src/
│   ├── app.rs                    # 装配运行依赖与 HTTP 服务
│   ├── config.rs                 # 进程配置
│   ├── network.rs                # 出站客户端、代理和自环检查
│   ├── api/cursor/               # Cursor HTTP/Connect 入口
│   │   ├── handlers.rs           # 路由选择、追加请求与官方转发
│   │   ├── bidi.rs               # hex/binary 解码、模型选择与改写
│   │   ├── run_sse.rs            # 下行订阅、终态和上游观测
│   │   └── proxy.rs              # 上游 HTTP 请求转发
│   ├── cursor/
│   │   ├── transport/            # request_id 对应的一次传输
│   │   │   ├── registry.rs       # 本地／上游路由及传输清理
│   │   │   ├── handle.rs         # 输入、订阅与终态操作
│   │   │   ├── inbox.rs          # append_seqno 去重和顺序释放
│   │   │   └── output.rs         # 输出广播、重放及关闭
│   │   ├── conversation/         # 会话中的运行与投递协调
│   │   │   ├── registry.rs       # conversation_id 对应的运行协调器
│   │   │   ├── runtime.rs        # 当前 Run、命令循环和传输绑定
│   │   │   ├── command.rs        # 会话命令与输出动作
│   │   │   ├── injection.rs      # 注入身份、去重及待提交状态
│   │   │   ├── pending.rs        # 运行边界上的待处理消息
│   │   │   ├── task.rs           # 子任务关联与后台完成投递
│   │   │   └── output.rs         # RunEvent 到客户端消息及检查点记录
│   │   ├── compile/              # 协议输入到领域消息、运行配置
│   │   ├── checkpoint/           # 检查点构建、恢复及持久化屏障
│   │   ├── tools/                # 工具执行与结果关联
│   │   │   ├── runtime.rs        # 工具参数、运行上下文与取消
│   │   │   ├── codec/            # Exec 请求、响应与工具卡片编码
│   │   │   ├── tool_call_dispatch/ # 本地、Exec、交互及 Await 分派
│   │   │   └── tool_call_result/ # 结果折叠、截断、图片与完成消息
│   │   ├── protocol/             # Connect 帧、protobuf 子集和下行事件
│   │   ├── prompting/            # 提示资产与稳定派生上下文编译
│   │   └── services/             # 循环之外的 Cursor 功能
│   │       ├── model_catalog.rs  # 本地／插件模型目录投影
│   │       ├── official_error.rs # 官方错误到子任务失败的转换
│   │       ├── knowledge/        # 规则持久化及客户端同步
│   │       └── usage.rs          # Token 总量与分类展示
│   ├── run/                      # Provider 无关的运行循环
│   │   ├── engine.rs             # 模型调用、工具轮与消息提交
│   │   ├── model_cycle.rs        # 单次模型事件流校验
│   │   ├── model_retry.rs        # 模型重试策略
│   │   ├── messages.rs           # 追加消息与事件幂等
│   │   └── compaction.rs         # 显式上下文压缩
│   ├── model/                    # 领域类型与模型参数规则
│   │   ├── directory.rs          # 统一模型目录和名称解析
│   │   ├── identity.rs           # 稳定模型身份
│   │   ├── selection.rs          # 本地／官方模型选择及参数
│   │   ├── configuration.rs      # 模型配置与保存时归一
│   │   └── tool_result_replay.rs  # 模型可见工具结果预算
│   ├── provider/                 # OpenAI、Anthropic、插件路由与调用记录
│   ├── plugin/                   # 插件描述、运行进程与账号资源
│   │   ├── registry.rs           # 插件资源所有权及执行循环
│   │   ├── selection.rs          # 候选排序、亲和、轮转与切换规则
│   │   ├── worker.rs             # Deno 子进程与调用关联
│   │   └── sdk/                  # 插件 TypeScript 契约
│   ├── store/                    # SQLite 持久化及事务
│   │   ├── models.rs             # 模型配置与原子批量更新
│   │   ├── settings.rs           # 类型化设置及锁内读改写
│   │   ├── checkpoints.rs        # 追加式检查点及消息身份
│   │   ├── background_completions.rs # 后台完成持久化与消费
│   │   ├── llm_calls.rs          # 模型调用及流式记录
│   │   ├── maintenance.rs        # 记录清理与 Blob 回收
│   │   └── migrations.rs         # schema 初始化与换行校验
│   ├── control/                  # 桌面与本机管理 HTTP 接口
│   │   ├── service.rs            # 管理操作编排
│   │   ├── discovery.rs          # 模型发现和凭据回填校验
│   │   ├── connectivity.rs       # 连通性测试与取消清理
│   │   ├── models.rs             # 模型 CRUD、分组 patch
│   │   ├── plugins.rs            # 插件资源与模型覆盖
│   │   └── test_support.rs       # 控制层测试夹具
│   ├── search/                   # 搜索及网页获取
│   └── local_app/                # Cursor 接管与本机集成
│       ├── proxy.rs              # 实际监听代理端口
│       ├── process.rs            # 本机进程操作
│       └── remote_ssh/           # 远端环境与技能部署
└── tests/                        # 传输、运行、中断、恢复及前缀跨模块验证
```

## 输入、运行、持久化与输出

```text
Cursor BidiAppend (hex 或 binary AgentClientMessage)
  → handlers/bidi：按首条模型选择本地或官方上游
    ├─ 官方：原字节转发；必须改写时保持编码并更新 Content-Length
    └─ 本地：TransportRegistry(request_id)
        → OrderedInbox(append_seqno)
        → ConversationRuntime(conversation_id，当前 Run 所有者)
        → compile(上下文、模型选择、消息身份)
        → RunEngine
            ├─ model cycle → Provider／插件 worker → 流式模型事件
            ├─ tool round → 本地／Exec／交互 → 关联工具结果
            ├─ message commit → Store 检查点、工具轮和调用记录
            └─ compaction → 替换模型历史，保留最新稳定上下文
        → conversation/output
            ├─ 检查点 worker → 持久化屏障 → checkpoint_update
            └─ AgentServerMessage → OutputHub → RunSSE → Cursor
```

RunSSE 的响应头是 `text/event-stream`，正文是 Connect 二进制信封：一字节 flags、四字节大端长度、payload；不是 `data:` 文本事件流。

`ExecServerMessage` 由服务端发送，客户端按 id 回报结果。执行中有 heartbeat，完成有 streamClose，异常有 throw，服务端取消发 abort。Shell 结果保留退出码、客户端提供的 signal 与完整输出位置；不自动读取输出文件。客户端审批仍保留，本项目不通过未知 Shell 字段执行管理员禁令。

## 身份与生命周期

- `request_id` 标识一次传输；断流重试建立新传输、新 inbox，序号从零开始。缺失序号未补齐时后续消息不执行；断连或生命周期结束清理，不能跳号。
- 客户端 `run_id` 可跨 attempt 保持稳定；`InjectContextAction.expected_run_id` 按此身份校验，而非新的请求 ID。
- `conversation_id` 绑定会话持久状态。子任务模型参数经官方 `model_id` 变体标识与子会话 `model_selection` 保留，不发送本地未知参数字段。
- InsertMessages 在同一 Run 内等待合适边界追加；BreakMessages 中断当前模型／工具 cycle 后追加并继续，不等于新建 Run。
- Finalizing 窗口的新消息进入待处理队列；需要下一轮时由会话运行协调器创建新 Run。取消、失败与正常结束均通过运行结果和终态路径处理。

```text
注入 → InjectionTracker.admit（稳定身份＋去重）
     → 编译领域消息 → enqueue → 中断当前 cycle／分离后台子任务
     → engine 追加提交 → take_committed → delivered 事件

取消／断连 → ConversationRuntime → RunHandle.cancel + Exec abort
          → RunOutcome → 最终检查点／终态 → 输出关闭与路由清理
```

## 模型与管理设置

- Effort 保存白名单为 `none/minimal/low/medium/high/xhigh/max`。每模型保留子集，去重保序；空子集填 `low/medium/high/xhigh/max`，失效默认取首项。
- 归一只在保存时发生；旧配置读取、插件动态描述符不自动洗库。额外请求参数是高级直通入口，保留覆盖能力。
- 分组界面只发送相对打开时快照真正修改的字段。省略表示保持服务器当前值；分组名空字符串表示清除。服务器单事务更新全部条目，任一失败回滚。
- 设置旧值读取与写入由同一写锁保护。自环校验使用实际监听端口，不以尚未更新的持久端口为依据。
- 插件选择策略集中在 `selection.rs`；资源、轮转计数及 worker 生命周期归 `registry.rs`。资源错误且尚未输出事件时才允许换候选，已输出后禁止重放。

## 历史与模块约束

无压缩时 Provider 可见消息是追加式历史：此前内容不修改、不重排。上下文变化以新事件追加；检查点编码与恢复保留事件身份。压缩是显式前缀重建，不由各 Provider 独立修补。

```text
api → cursor → run → model/provider/store
cursor → compile/checkpoint/tools/protocol
control → model/plugin/store/local_app
```

`run/provider/store/model` 不反向依赖 `cursor`。协议 schema 来自实际客户端提取，不回填本地扩展；当前验证基线为 Cursor 3.14.27。升级先向临时目录提取并检查类型、字段、枚举、RPC，再同时更新消费者及生成产物。

## 验证入口

- `bidi_forwarding`：hex/binary、旧长度头重编码、官方转发及冲突路由错误。
- `transport_reconnect`、inbox 单测：序号空洞、断连清理及新传输。
- `interrupt`、`conversation_delivery`：注入、稳定 run 身份、后台结果及取消。
- `prefix_stability`、`checkpoint_recovery`、`compaction`：追加历史与恢复。
- `store/models`、`store/settings`：保存归一、分组原子性、密钥占位及并发设置。
- `plugin`：候选策略、worker 协议和跨模块失败切换。
- Windows CI 运行桌面更新替换测试；Linux CI 运行工作区测试及提取器验证。
