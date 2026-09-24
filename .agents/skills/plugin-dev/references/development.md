# 构建、联调与验证

仅在准备工程、构建或验证插件时读取。这里给出执行入口，不复制 SDK 协议或安装 API 的字段手册。

## 工程与依赖

| 位置 | 用途 |
| --- | --- |
| 主仓 `backend/crates/gateway-plugin/sdk/` | Rust 公开合同、会话辅助与能力文档 |
| 主仓 `backend/apps/plugin-cli/` | `cpr-plugin package`，只校验与打包，不编译插件源码 |
| 独立 `codex-proxy-plugins/examples/workbench/` | 基础能力体验与文本工作流示例，按需选择功能，不整体复制 |
| 独立 `codex-proxy-ui/` | 可选的 Vue UI 库、组件文档与 playground |

先检查用户给定工程，或主仓父目录下的同级仓库是否存在；读取实际仓库约定、README、`Cargo.toml`、`package.json` 与锁文件。
只需中间件时不复制示例的管理 API、页面和相关权限；Provider、CLI 等能力从 SDK 对应合同起步，不给示例添加无关能力。

本地源码联调可以使用 Cargo 路径依赖和 pnpm 的包级链接／override；路径必须真实可解析，并记录在消费方依赖配置中。
这不等于允许从业务源码跨仓 `import .../src/...`，也不能证明第三方能独立安装。

对外交付前核实 UI／SDK 的可用版本和安装来源：仅引用确实存在的发布版本，或包含所需合同的固定 Git commit；
不要把浮动 `main` 当版本，也不要自动发布依赖以补齐条件。本地 override 尚未移除时，明确说明独立安装缺口。
所用工具链以目标项目的配置为准，不因本机安装了更新版本而顺手升级。

## 复用示例

找到 `codex-proxy-plugins` 后先读根目录与 `examples/workbench/README.md`。
该仓库使用 `bash scripts/package` 构建页面、Rust 二进制并打包；前端依赖和工具配置位于示例的 `frontend/`，根目录不维护 Node 工程。执行前核对脚本适用的插件路径和目标平台。

| 任务 | 示例入口 |
| --- | --- |
| 身份、声明与资源 | `examples/workbench/plugin.json` |
| 会话启动与处理器组合 | `examples/workbench/backend/src/main.rs`、`app.rs` |
| 管理接口与 Provider | `examples/workbench/backend/src/management/`、`provider/` |
| 页面与主题 | `examples/workbench/frontend/src/` |
| 管理接口调用 | `examples/workbench/frontend/src/api/modules/` |
| 包内静态资源构建 | `examples/workbench/frontend/vite.config.ts` |

若派生新插件，同时修改包名、二进制名、插件身份、扩展项 ID、注册描述和构建目标，不只改展示名称。
保留当前项目确实需要的依赖、资源和权限；不要把示例中的全请求范围直接用到共享环境。

## 构建与打包

先运行目标工程已有的测试和构建入口。Rust 检查示意如下，路径、工具链和 target 按实际工程替换：

```bash
cargo fmt --manifest-path /path/to/plugin/Cargo.toml -- --check
cargo clippy --manifest-path /path/to/plugin/Cargo.toml --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path /path/to/plugin/Cargo.toml --locked
cargo build --manifest-path /path/to/plugin/Cargo.toml --release --locked --target x86_64-unknown-linux-gnu
```

新工程尚无锁文件时先生成并检查锁文件；不要把缺少锁文件的 `--locked` 失败当作源码问题。
有页面时另跑该前端项目的检查与生产构建，确认产物路径、资源清单和 MIME 类型相符；无页面的插件无需 Node 项目。

优先使用已有 `cpr-plugin`。需要本地构建工具时，在主仓根目录运行以下命令，把 `--help` 换成实际参数即可，不必全局安装：

```bash
cargo run --manifest-path backend/Cargo.toml -p codex-proxy-plugin-cli --locked -- package --help
```

直接打包的示意命令：

```bash
cpr-plugin package \
  --manifest /path/to/plugin/plugin.json \
  --binary /path/to/plugin/target/x86_64-unknown-linux-gnu/release/plugin \
  --target x86_64-unknown-linux-gnu \
  --resource-map web=frontend/dist \
  --output-dir /path/to/plugin/dist
```

没有 `web` 资源时去掉 `--resource-map`。映射源相对于作者清单目录，而非命令执行目录；资源必须留在插件工程内。
目标平台与二进制必须匹配，不能只改 target 参数伪装交叉编译。支持平台以 [CLI 文档](../../../../backend/apps/plugin-cli/README.md)及实际 `--help` 为准。

输出为 `.tar.gz` 与 `.sha256`，不是源码 zip、npm 包或页面目录。归档根目录应有生成后的 `plugin.json`、可执行文件和声明资源。
本地同版本反复试验与对外发布区分处理；发布新内容使用明确的新版本，不覆盖已发布版本。

## 验证与交付

按[插件使用](../../../../docs/plugins.md)验证“安装包 → 查看权限并安装 → 自动准备默认配置 → 实际使用”；只在缺少业务必填值时补配置，不添加授权或资源选择向导。
只有获得目标环境操作授权才安装和启用；不把修改源码的授权扩展到生产实例或真实凭据操作。
需要 API 自动化时查[插件管理 API](../../../../docs/api.md#12-插件管理)，不要依据记忆拼路由。

按实际实现选择验证，不为不存在的能力增加测试任务：

| 实现范围 | 应验证的行为 |
| --- | --- |
| 中间件 | 匹配／不匹配范围、授权不足、下游错误与取消；支持流式时检查多帧、终态和断开；若涉及续链，使用真实目标客户端检查 HTTP/SSE、WS 及后续轮次 |
| Provider | 导入或登录、模型发现、实际执行、错误及用量；仅对声明支持的协议、资料、额度等操作验收 |
| 管理页面 | 从宿主打开，标题／副标题、明暗主题、加载与错误状态；管理桥请求成功；停用或版本切换后旧入口失效 |
| 入口认证／CLI／维护 | 对应身份和账号范围、缺少授权、取消与错误；不把帮助输出、注册成功或一次健康探测当完整验收 |
| 持久状态或升级 | 读写版本冲突、升级兼容性、迁移失败与停用边界；不覆盖并发配置或把回滚版本误当恢复历史数据 |

真实请求按用户选择的客户端、模型和测试预算执行；没有这些能力或授权时列为缺口，不模拟成功。
临时账号、Key 或配置只在明确的测试范围内创建，记录并清理自己创建的对象，不输出密钥、原始请求或插件 secret。

交付简述：源码与产物位置、兼容范围、所需能力／权限／绑定、执行过的命令及结果、尚未验证的场景。
只有确实生成了产物才报告“已打包”，只有目标环境业务验证成功才报告“已生效”；提交、推送与发布按用户授权另行执行。
