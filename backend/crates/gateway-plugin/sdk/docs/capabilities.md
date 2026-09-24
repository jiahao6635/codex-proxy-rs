# 插件能力与回调

[返回 SDK](../README.md) · [清单与图标](manifest.md)

`contributes` 声明插件提供什么功能，`permissions` 声明插件需要访问哪些宿主资源。两者不是同一件事：
管理页面可以调用现有模型而不实现 Provider；Provider 也只有在需要额外访问宿主资源时才声明对应权限。

## 类型化作者入口

启用 `io` feature 后，组合插件优先使用 `client::PluginBuilder`：

```rust,ignore
use gateway_plugin_sdk::client::{PluginBuilder, methods};

let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
    .management(management_registration(), handle_management)?
    .command_line(command_registration(), handle_command)?
    .on(methods::ROUTE_MODEL, route_model)?
    .build()?;

session.run(plugin).await?;
```

`PluginBuilder::from_json` 与 `cpr-plugin package` 使用同一个作者清单规范化入口。构建器负责：

- 自动响应 `plugin.register`，注册内容直接来自规范化后的清单
- 按方法合同解码控制参数和二进制载荷，并在进入业务处理器前检查调用阶段
- 保留 `TypedCall` 中的调用上下文、宿主客户端和取消信号
- 按方法合同编码 `TypedReply`，流式结果继续复用有界 `ResponseStream`
- 在 `build()` 时核对能力、必需处理器及 Provider 描述，避免清单与分派表漂移

`management`、`command_line` 和 `middleware` 是常用组合入口；其余能力使用
`on(methods::..., handler)`。方法常量固定请求、响应、阶段与载荷位置，作者不手写 RPC 方法名。
状态迁移不是 capability：清单任一 `state[]` 声明了 `migratesFrom` 时，注册
`methods::STATE_MIGRATE`，构建器也会要求该处理器存在。

类型化合同故意区分三种数据：普通控制元数据、可能敏感的 JSON 二进制载荷、原始字节。
例如 CLI 参数、账号凭据和状态迁移批次使用二进制 JSON，管理 HTTP 正文保持原始字节，
Provider 准备输入使用 `GPQ1`。错误地把敏感载荷放进控制参数会在业务处理器运行前被拒绝。

## 能力与方法

Provider 业务组包含 `executor`，并可选择 `models`、`authentication`、`quota`、
`request_profile`、`account_management`、`billing`、`maintenance`。作者清单的简写见
[Provider 业务组](manifest.md#provider-业务组)。独立能力如下：

| 能力 | 类型化方法 |
| --- | --- |
| `middleware` | `PluginBuilder::middleware` / `middleware.handle` |
| `model_router` | `methods::ROUTE_MODEL` |
| `scheduler` | `methods::SCHEDULE_ACCOUNT` |
| `request_lifecycle`、`usage` | `methods::OBSERVE_REQUEST`，同一实例的终态订阅合并调用 |
| `web_socket_observer` | `methods::OBSERVE_WEBSOCKET`，线方法为 `websocket.response_event` |
| `frontend_authentication` | `methods::FRONTEND_IDENTIFIER`、`methods::FRONTEND_AUTHENTICATE` |
| `management` | `PluginBuilder::management`；公开回调另用 `methods::MANAGEMENT_CALLBACK` |
| `command_line` | `PluginBuilder::command_line` |

## 五个访问域

安装时接受清单声明的访问域，不存在另一套逐方法、逐用途或资源 ID 白名单。宿主仍会检查父调用、阶段、
期限、实例代次、资源归属，以及 Key、账号和 Provider 自身的业务规则。

| 权限 | 开放的宿主能力 |
| --- | --- |
| `network` | 通过 `host.http.*` 使用宿主受管出站网络；仍受统一代理、超时、大小和流控规则约束 |
| `models` | 列出非秘密 Key、按所选 Key 查询模型，以及通过 `host.model.*` 调用模型；调用可能产生消耗 |
| `accounts` | 查询账号、读取原始凭据及创建或替换账号；写入仍经过 revision CAS、审计和发布事务 |
| `requests` | 参与请求／响应处理、路由、调度、观察及亲和查询 |
| `public_endpoints` | 提供无需登录即可访问的已声明静态资源或一次性票据回调 |

`host.log` 和本插件声明的 `host.state.*` 是基础设施，不需要额外 permission。公开调用阶段不开放任何
宿主回调；权限也不能把一个父调用的句柄、流或上下文转移到另一个调用。

## Provider

调用 `.provider(ProviderDescriptor)` 冻结 Provider 的模型、认证操作、账号操作、计费与画像描述。
`build()` 会要求 `executor` 的准备、执行、释放和连接测试处理器齐全，并根据描述符中选择的操作要求
对应凭据、账号、模型、额度或画像处理器；描述符与能力不一致会直接失败。
Provider 执行使用宿主选择的账号和凭据，因此 Provider 清单需要 `accounts` 访问域；需要宿主代发 HTTP 时再声明
`network`。这里的权限开放宿主端口，不替代 Provider 描述和处理器声明。

Provider 执行分为：

1. `provider.prepare` 在 `attempt` 阶段接收 `ExecutionInput`，只准备执行并返回 `PreparedExecution`，不能发送推理请求。
2. `provider.execute` 在 `execution` 阶段使用准备 token，返回 `ResponseStream`；SDK 固定初始结果为 `{"stream":true}`。
3. `provider.discard` 在 `attempt` 阶段释放没有执行或已经结束的准备 token。
4. `provider.connection_test` 在 `configuration` 阶段生成由宿主执行的探测操作。

`ExecutionInput` 与 `ExecutionEvent` 分别使用 `GPQ1`、`GPE1` 长度编码，把请求正文、原始 wire 和失败正文
保留在独立二进制段。流必须遵守宿主授予的字节／帧信用，不能用分片绕过单片、聚合总量或终态检查。
`PrepareExecution.headers` 只包含 Attempt 中间件追加并经宿主复核的业务 header，不能覆盖认证、目标或传输字段。

静态模型放在 `ProviderDescriptor.models`；声明 `models` 时还要提供账号模型发现描述和
`provider.models`。认证操作由 `credential_operations` 决定；账号资料、订阅、头像和重置卡由
`account_operations` 决定。插件只返回准备事实，账号 ID 分配、凭据 revision、提交事务、计费和审计仍由宿主拥有。

`continuation_state` 可声明稳定格式、当前写版本和可读版本集合。兼容声明只允许当前执行器解释旧检查点，
不会放宽插件、实例、配置、账号、凭据 revision 或当前请求身份的复核。

## 宿主资源回调

所有回调都通过当前 `TypedCall.host` 发起，并受父调用截止时间和取消信号约束。

### 账号与凭据

`accounts` 域开放 `host.auth.list/get_runtime/get/save`：

- `list` 可用 `provider_id` 过滤并分页，返回不含原始凭据的运行投影
- `get` 的原始凭据只放在二进制载荷中
- `save` 新建时显式提供 Provider，由宿主生成账号 ID；替换时必须提交精确 credential revision

插件可以自行选择其业务需要的账号和 Provider，不需要安装器预先配置允许列表。宿主仍校验账号归属、资格、
凭据版本和并发事务；原始凭据不得进入日志、审计正文或控制元数据。

### Key、模型与模型调用

`models` 域开放：

- `host.keys.list`：只返回 `{id, name, enabled}`，不返回 Key 明文、前缀或隐藏 scope
- `host.models.list`：以显式 `client_key_id` 查询该 Key 当前可见的模型目录
- `host.model.execute[_stream]`：交给 Core 的路由、准入、租约、重试、账本和计费链路执行
- `host.model.stream_read/close`：读取或关闭当前父调用创建的有界流

`management`、`command_line`、`maintenance`、`authentication` 和 `observation` 阶段调用模型时，在
`ModelExecuteRequest.client_key_id` 中显式选择执行 Key；`request`、`attempt`、`execution`、`routing` 和
`scheduling` 阶段将该字段留空并继承父请求身份，不能切换 Key。Key 的启用状态、模型规则、账号组、并发和预算
始终使用调用时的当前事实。
插件不能读取 Key 明文，也不能用一个 Key 查询结果替另一个 Key 执行。

非流式模型结果通过 `ModelEventBatch` 的 `HME1` 二进制批次返回；流式句柄只在创建它的父调用内有效。
发起回调的插件实例在子调用中跳过，Core 还会限制递归深度、总数和并发，避免插件调用自身形成环。

`host.affinity.lookup` 属于 `requests` 域，只查询 Provider 已持久化的真实亲和键。命中只是账号偏好，
后续执行仍重新检查当前 Key 规则、账号资格和租约。

### 网络

`network` 域通过 `host.http.do/do_stream` 和流读取／关闭方法提供受管 HTTP。URL、header 和正文可能含敏感数据，
正文使用独立二进制载荷。宿主统一施加代理、超时、帧与流限制；插件不能绕过宿主端口取得额外调用身份。
地址被拒绝返回 `permission_denied`，期限耗尽返回 `timeout`，解析、连接或响应读取失败返回 `upstream`。
这些 HTTP 业务错误的 `message` 是宿主生成的安全分类提示，不包含目标 URL、凭据或上游错误正文，
插件可以在相应操作中展示；协议故障不应直接展示内部消息。Fake-IP 环境按[部署排障](../../../../../deploy/README.md#插件取文与-fake-ip-dns)
调整 DNS，不通过放宽地址范围解决。

## 请求扩展

### 洋葱中间件

`middleware` 的 `request` mount 在逻辑请求外层执行一次，`attempt` mount 在每次选定 Provider 和账号后重建。
作者必须在清单中显式选择一个或两个阶段。`PluginBuilder::middleware` 接收 `MiddlewareCall`，其中
`next.run(request)` 是 single-use continuation；响应按相反顺序穿过中间件。

请求正文和响应帧受 `requests` 域保护。没有正文时仍要区分“保持原正文”和“替换为空”：默认使用
`MiddlewareRequestBody::Preserve`，只有 `replace_body(Vec::new())` 才表示清空。Header 修改采用增量
remove/append，保留合法多值。`MiddlewareBody::map_frames` 复用会话的流控和唯一终态，不建立插件全局流表。

仅实现中间件时也可使用轻量的 `MiddlewarePlugin`；需要与管理、Provider 或其他方法组合时使用
`PluginBuilder`，两者复用相同的 `MiddlewareCall` 和 `MiddlewareResponse`。

### 路由、调度与观察

`policy.route_model` 和 `policy.schedule_account` 只提出决定，宿主随后复核 Key 规则、模型能力、账号资格和租约。
`policy.observe_request` 接收一次最终请求事实；`request_lifecycle` 提供终态，`usage` 提供最终标准化用量、费用和耗时。
缺失值表示宿主没有该事实，不等于零，也不能由插件重新计价。

`websocket.response_event` 只读观察真实上游 WS 事件，元数据与原始帧分开传输。事件序号在请求内单调递增；
有界队列丢弃可能产生间隔。观察超时、过载或插件失败不改变业务响应，插件也不能修改 WS 帧。

观察计划在请求开始时冻结，最终事件最多派发一次。`Handshake`、`Message`、`Frame` 和 `PluginFault` 的
调试输出会隐藏配置、载荷与错误正文；插件自己的日志同样不能输出凭据或完整请求。

## 状态、日志与迁移

清单 `state[]` 声明命名空间、`schemaVersion`、JSON schema、记录与字节配额。运行时用
`host.state.get/put/delete` 访问；创建要求键不存在，更新和删除必须提供精确版本。状态始终绑定当前
插件实例、制品和 schema，不能直接读写宿主数据库。

不兼容升级由宿主停用并排空旧会话，再调用 `plugin.state.migrate` 分批迁移到暂存状态。插件逐项返回
keep、replace 或 delete；全部完成后宿主才原子提交新配置与状态，失败时丢弃暂存结果。

`host.log` 接收固定事件名、级别和有界字段。宿主脱敏并限制速率；`recorded: true` 只表示已交给有界日志任务。
日志和私有状态无需 permission，但注册、配置与公开管理阶段仍受各自的回调生命周期限制。

## 管理页面、公开入口与 CLI

`PluginBuilder::management` 同时冻结 `ManagementRegistration` 并绑定 `management.handle`。请求元数据在控制参数，
原始 HTTP 正文在二进制载荷；响应同样分离。宿主不转交管理 Cookie、Authorization 或任意 header，插件也不能
返回任意响应 header。账号操作通过 `accounts` 域调用宿主，资源由调用参数选择，不在路由注册中固定 Provider。

页面运行在隔离 iframe 中，静态资源必须同时出现在清单 `resources` 和管理注册中。标记为公开的资源及一次性
票据回调需要 `public_endpoints`；`management.callback` 使用 `public_management` 阶段，并且不能调用账号、
网络、模型、状态或日志端口。

宿主自动注入尺寸监听与上报脚本，为 `body` 设置扣除页面头部和外侧留白后的最小高度，整页滚动由管理端承接。
插件无需调用 resize、发送尺寸消息或安装桥接依赖；Vue 等框架的根容器需要延续最小高度时，使用普通 CSS `min-height: inherit`。
页面使用自然文档流，内容增加或减少时自动同步高度；不使用 `100vh`、`height: 100%`、整页固定定位或内部滚动容器代替文档流。
编辑器、日志等局部区域可自行限高滚动，脱离文档流的浮层不会用于撑高页面。

### 页面宿主桥 v2

宿主注入只读的 `window.codexProxyPlugin`。`request({ method, path, query?, contentType?, body? })` 只能访问当前版本
已注册的管理路由，返回 `{ status, contentType, body: ArrayBuffer }`；`callbackTicket({ path, ttlSeconds })` 为已注册
回调签发 1–600 秒的一次性地址；`resourceUrl(path)` 只解析同一制品已注册的非 HTML 资源。`theme` 为当前
`light` / `dark` 值，变化时触发 `codex-proxy-themechange`。页面不能指定其他实例、制品、版本、URL 或请求 header。

普通模型请求使用标准 `Response` 接口：

```ts
interface PluginModelsBridge {
  responses(input: {
    clientKeyId: string
    body: Record<string, unknown>
    signal?: AbortSignal
  }): Promise<Response>
}

interface CodexProxyPluginBridge {
  readonly version: 2
  readonly models: PluginModelsBridge
}

declare global {
  interface Window {
    readonly codexProxyPlugin: CodexProxyPluginBridge
  }
}

const controller = new AbortController()
const response = await window.codexProxyPlugin.models.responses({
  clientKeyId: selectedClientKeyId,
  body: { model: 'gpt-5', input: 'Hello', stream: true },
  signal: controller.signal,
})
if (!response.ok)
  throw new Error(await response.text())
for await (const chunk of response.body!) {
  // chunk 是原生 Responses JSON 或 SSE 字节流的一部分。
}
```

`clientKeyId` 是管理端 Key 标识，不是 Key 原文；页面可通过自己的已注册管理路由调用 `host.keys.list` 和
`host.models.list` 获取 Key 候选及其可见模型，但桥不会读取或返回秘密。该请求进入普通 Responses 数据面，
因此当前实例自己的 middleware、router、scheduler 和
observer 也按常规计划执行；它不是插件执行期间的嵌套 `host.model` 回调。正文最多 8 MiB，每页最多 4 个并发模型
请求，总时限 10 分钟；响应以不超过 64 KiB 的单块 pull/ack 传输。页面 30 秒不继续消费响应、调用
`AbortController.abort()` / `ReadableStream.cancel()`，或页面被切换、停用、换版时，宿主都会取消请求。桥不接受
任意 URL、header 或 Key 原文；只转交响应状态、安全 header 以及原生 JSON / SSE 正文。

`PluginBuilder::command_line` 冻结命令帮助并绑定执行函数。注册和执行的 JSON 都位于二进制载荷，控制信封为 `{}`。
命令若返回待保存账号，需要 `accounts` 域；只有退出码为零时宿主才按顺序执行各自独立的 CAS 提交。
模型命令通过 `host.keys.list` 展示非秘密 Key，再显式选择 Key 查询模型和执行，不依赖安装时预绑定。

## 数据面入口认证

`frontend_auth.identifier` 返回稳定认证器标识；`frontend_auth.authenticate` 接收敏感的 Authorization 载荷并返回
外部 principal。插件不能指定 Client Key，宿主根据当前入口配置完成映射，并继续检查 Key 的启用状态、模型规则、
账号组、并发和预算。该能力只处理数据面身份，不接管管理端登录。
