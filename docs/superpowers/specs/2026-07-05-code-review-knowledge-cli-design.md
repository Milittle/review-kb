# Code Review 外挂知识库 CLI 设计方案

日期：2026-07-05  
状态：待评审  
CLI 名称（本设计）：`review-kb`

## 1. 背景

在 MR 检视开始前，检视流程需要加载当前项目的自定义 checklist。CLI 将固定格式的 `review-checklist.md` 解析为稳定的项目检视描述，并把完整规则存入本地 SQLite 数据库。

Agent 首先读取包含全局说明和规则概要的 description，从中选择与本次 MR 相关的规则 key；随后通过 CLI 批量取得这些 key 对应的完整规则，并据此执行检视。CLI 不调用模型，也不使用向量检索，所有构建和查询均为确定性的字符串处理。

## 2. 目标

- 使用一个可配置路径的本地 SQLite 数据库管理多个 CodeHub 项目。
- 以 CodeHub 稳定仓库 ID 作为项目唯一标识，允许项目名称变化。
- 在 MR 检视入口自动判断知识库是否可用，必要时原子重建。
- 根据 checklist 的声明版本和文件内容哈希检测变化。
- 确定性生成供 Agent 使用的 description。
- 支持按项目和规则 key 精确、批量查询完整规则。
- 支持按概要、正文、标签、路径和语言进行定位查询。
- 支持离线诊断、备份、恢复和受控的紧急规则覆盖。
- 所有命令统一返回 JSON，供 Agent 和自动化流程稳定消费。

## 3. 非目标

第一版不包含以下能力：

- CLI 内部调用 LLM 或判断 MR 应使用哪些规则。
- Embedding、向量数据库或语义检索。
- CLI 主动调用 CodeHub API、处理 CodeHub 凭据或读取 MR。
- 把 SQLite 数据库提交到项目仓库。
- 多机器数据库同步或中心化服务。
- 任意 SQL 写入接口。所有写操作必须经过领域校验，避免破坏数据库约束。

## 4. 核心术语

- `project_id`：由检视流程从 CodeHub 获取并传给 CLI 的稳定仓库 ID，全库唯一。
- `project_name`：项目展示名称，可变，不参与规则唯一性判断。
- `rule_key`：checklist 内人工维护的稳定规则标识，只需在单个项目内唯一。
- `qualified_key`：对外展示的限定 key，格式为 `<project_id>:<rule_key>`。
- `source rule`：从 `review-checklist.md` 导入的原始规则。
- `override`：针对某条 source rule 的本地受控覆盖，用于紧急修复。
- `description`：全局检视说明和全部规则概要的确定性 JSON 表示，不包含规则正文。
- `knowledge_revision`：当前有效知识内容的确定性 SHA-256，用于保证 Agent 筛选与规则读取基于同一快照。
- `selection`：Agent 返回给 CLI 的结构化选择单，包含 project ID、knowledge revision 和精确 rule key 数组。
- `prepare`：检视入口调用的“确保知识库已就绪”操作。

数据库内部使用 `(project_id, rule_key)` 作为规则复合唯一键，不把项目名称直接拼入持久化 key。项目改名只更新元数据，不触发规则重建。

## 5. 总体架构

CLI 分为六个边界清晰的模块：

1. **CLI Adapter**：解析命令参数，输出统一 JSON，映射退出码。
2. **Checklist Parser**：解析并严格校验固定格式 Markdown，产出领域对象。
3. **Knowledge Service**：实现 prepare、同步、查询、override 和冲突处理。
4. **Description Builder**：根据有效规则确定性生成 description。
5. **SQLite Repository**：事务、查询、迁移、备份和并发控制。
6. **Configuration**：解析数据库路径和其他本地配置。

依赖方向为 CLI Adapter → Knowledge Service → Parser/Builder/Repository。Parser 和 Builder 不依赖 SQLite，可通过纯输入输出单独测试。

### 5.1 技术基线

- Python 3.10 或更高版本；当前项目环境为 Python 3.10，类型和标准库用法以此为兼容基线。
- Python 标准库 `sqlite3` 提供嵌入式数据库能力，不依赖独立数据库服务。
- Typer 定义命令和参数；CLI Adapter 统一接管结果序列化，避免 Typer 的展示格式进入 stdout 协议。
- `markdown-it-py` 读取 Markdown token，PyYAML `safe_load` 解析 YAML，Pydantic 校验解析后的领域对象。
- 内置、顺序化 SQL migration 管理数据库 schema；第一版不引入面向服务端数据库的迁移框架。
- pytest 覆盖单元、集成和 CLI 契约测试。

项目发布为标准 Python package，并提供 `review-kb` console script。核心业务 API 不依赖命令行上下文，便于测试和未来被 Python 流程直接调用。

## 6. MR 检视数据流

### 6.1 准备阶段

检视流程执行：

```bash
review-kb prepare \
  --project-id "123456" \
  --project-name "payment-service" \
  --checklist "/repo/review-checklist.md"
```

CLI 按以下顺序处理：

1. 读取并解析 checklist，计算原始文件 SHA-256。
2. 校验 schema、checklist 版本、全局描述、rule key 和规则字段。
3. 查询 `project_id` 对应的已存快照。
4. 数据不存在时构建；声明版本或内容哈希变化时刷新；两者均一致时复用。
5. 若内容变化但声明版本未变化，完成刷新，同时在结果中返回警告。
6. 在单个事务中写入项目、规则、快照和 description。
7. 返回当前有效 description。

“解析、校验和冲突分析”在写事务前完成。写入使用单事务替换，失败时保留上一个完整快照，不暴露半构建状态。

### 6.2 规则选择与读取

Agent 根据 prepare 返回的全局描述、规则概要和筛选字段选择 key。自动化流程必须使用结构化选择单，通过 stdin 传给 CLI：

```bash
review-kb rules get --input - <<'JSON'
{
  "project_id": "123456",
  "knowledge_revision": "sha256:...",
  "keys": ["SEC-001", "DB-004", "API-012"]
}
JSON
```

CLI 按输入 key 顺序返回有效规则。有效规则等于 source rule 应用 active override 后的结果。CLI 必须先整体校验项目、知识库 revision、key 格式、重复项和存在性；全部通过后才返回规则正文。任一 key 不存在时整个命令失败，避免 Agent 在规则不完整时继续检视。

### 6.3 Agent 选择单与失败恢复

`prepare` 返回 `selection_context`：

```json
{
  "project_id": "123456",
  "knowledge_revision": "sha256:..."
}
```

`knowledge_revision` 是当前有效知识的 SHA-256，包括 source checklist 和 active override 的规范化结果。它不同于 checklist 的 `content_hash`：后者只表示源文件，前者表示 Agent 实际看到并将要读取的有效规则集合。

Agent 只能从 description 的 `rules[].key` 原样复制 key。给 Agent 的 description 不同时暴露 `qualified_key`，避免混用本地 key 和带项目前缀的展示 key。项目隔离由选择单中的 `project_id` 保证。

错误恢复规则：

- key 不存在：返回 `RULE_NOT_FOUND`、全部 `not_found` key 和每个 key 最多三个确定性候选建议。建议优先级为忽略大小写精确匹配、前缀匹配、编辑距离。
- revision 不一致：返回 `KNOWLEDGE_REVISION_MISMATCH`，调用方必须重新执行 `prepare` 并让 Agent 重新选择。
- key 重复或格式非法：返回 `INVALID_SELECTION`，指出数组位置和原因。
- CLI 不自动采用候选建议，也不在失败响应中返回部分规则正文。Agent 修正选择单后可重新调用一次；仍失败则停止本次规则加载并报告错误。
- 候选建议仅用于纠错，不改变 key 的大小写敏感精确匹配语义。

人工操作可以使用 `--project-id` 和重复的 `--key` 参数，但 Agent 接入必须使用 `--input -` JSON 协议，避免逗号拆分、shell 引号和空白格式问题。

## 7. `review-checklist.md` 格式

### 7.1 文件级元数据

文件使用 YAML Front Matter，必填字段如下：

```markdown
---
schema_version: 1
checklist_version: "2026.07.1"
global_description: |-
  检查本项目的安全性、事务边界、兼容性和可观测性。
---
```

- `schema_version`：格式版本，第一版只接受整数 `1`。
- `checklist_version`：项目维护者声明的非空字符串版本。CLI 不假设 SemVer，也不比较大小，只比较是否相等。
- `global_description`：提供给 Agent 的全局检视说明，必须为非空字符串。
- `project_id` 不写入 checklist，以 CodeHub 调用上下文为准，避免仓库复制或迁移产生错误身份。

### 7.2 规则格式

每条规则由二级标题、一个 `review-rule` YAML 代码块和 Markdown 正文组成：

````markdown
## SEC-001

```yaml review-rule
summary: 检查外部输入是否在进入 SQL 前完成参数化处理
tags:
  - security
  - database
paths:
  - "src/**/*.py"
languages:
  - python
```

### 检视要求

禁止使用字符串拼接构造 SQL。确认查询通过驱动提供的参数绑定接口执行，
并检查动态表名、排序字段等无法直接绑定的位置是否经过白名单限制。
````

字段定义：

| 字段 | 必填 | 约束 |
|---|---:|---|
| 标题中的 `rule_key` | 是 | 项目内唯一且区分大小写；匹配 `[A-Za-z0-9][A-Za-z0-9._-]{0,127}`；禁止两个 key 仅大小写不同 |
| `summary` | 是 | 非空单行字符串，用于 description 和 Agent 初筛 |
| `tags` | 否 | 去重后的字符串数组，默认 `[]` |
| `paths` | 否 | Git 风格 glob 字符串数组，默认 `[]`；仅作为 Agent 筛选提示，CLI 第一版不匹配 MR 文件 |
| `languages` | 否 | 小写语言标识数组，默认 `[]` |
| Markdown 正文 | 是 | 非空，作为完整检视规则返回 |

规则按文件中的顺序生成 description。正文内可使用三级及更深标题，但不得再出现二级标题。未知字段、重复 key、多个规则元数据块、空正文或非法类型均视为格式错误。

## 8. Description 设计

Description 不依赖模型，使用解析后的规范化数据确定性生成。其逻辑结构为：

```json
{
  "project": {
    "id": "123456",
    "name": "payment-service"
  },
  "checklist": {
    "schema_version": 1,
    "version": "2026.07.1",
    "content_hash": "sha256:...",
    "knowledge_revision": "sha256:..."
  },
  "global_description": "检查本项目的安全性、事务边界、兼容性和可观测性。",
  "rules": [
    {
      "key": "SEC-001",
      "summary": "检查外部输入是否在进入 SQL 前完成参数化处理",
      "tags": ["security", "database"],
      "paths": ["src/**/*.py"],
      "languages": ["python"]
    }
  ]
}
```

生成规则如下：

- 保留 checklist 中规则顺序。
- 数组字段去重但保留首次出现顺序。
- 不注入时间戳等不稳定字段到 description 本体。
- active override 修改概要或筛选字段时，description 使用 override 后的有效值。
- `knowledge_revision` 根据规范化后的全部有效规则生成，任何有效字段变化都会改变该值。
- description 本身存入数据库，以便查询和诊断；可随时由当前有效规则重建。

`knowledge_revision` 是以下规范化 JSON 的 SHA-256：project ID、checklist schema/version/content hash，以及按 ordinal 排列的全部有效规则字段。它包含 active override 的结果，不包含项目名称、数据库路径或时间戳。因此项目改名不会让选择单失效，而规则内容或 override 变化一定会产生新的 revision。

## 9. CLI 命令面

### 9.1 检视流程命令

| 命令 | 作用 | 是否写库 |
|---|---|---:|
| `prepare` | 确保项目知识库与 checklist 一致并返回 description | 可能 |
| `status` | 比较指定 checklist 与已存快照，返回 `missing/current/stale/conflict/invalid` | 否 |
| `sync` | 显式同步指定 checklist | 是 |
| `rebuild` | 忽略已有派生数据，从源文件重建 | 是 |

`prepare` 是自动化流程的主要入口，调用方不需要自行编排 `status + sync`。

### 9.2 项目与规则查询

| 命令 | 作用 |
|---|---|
| `projects list` | 列出所有项目，支持按 ID、名称过滤 |
| `projects show --project-id` | 显示项目、当前版本、哈希、规则数量和同步状态 |
| `description get --project-id` | 读取当前有效 description |
| `rules list --project-id` | 列出规则概要和筛选字段 |
| `rules get --input -` | 校验结构化选择单并按输入顺序批量读取完整有效规则 |
| `rules search --project-id --query` | 对 key、summary、content、tags、paths、languages 做大小写不敏感的字面量包含查询 |

第一版搜索是确定性的字面量搜索，不承诺自然语言相关度排序。返回顺序为规则原始顺序。

### 9.3 Override 管理

| 命令 | 作用 |
|---|---|
| `overrides set` | 为指定规则设置字段级覆盖，必须填写原因 |
| `overrides show` | 查看 source、override 和最终有效值 |
| `overrides list` | 列出 active/conflict/disabled override |
| `overrides unset` | 禁用 override，恢复 source rule |
| `overrides resolve --keep-override` | 源规则变化后，确认继续使用 override 并更新基线 |
| `overrides resolve --accept-source` | 丢弃 override，接受新 source rule |

允许覆盖 `summary`、`content`、`tags`、`paths` 和 `languages`，不允许覆盖 `project_id` 或 `rule_key`。设置和处理 override 后都重新生成 description。

### 9.4 数据库运维

| 命令 | 作用 |
|---|---|
| `db info` | 显示数据库路径、schema 版本、项目和规则计数 |
| `db check` | 执行 SQLite 完整性检查和领域一致性检查 |
| `db query` | 对允许的项目、规则、同步和审计视图执行只读查询 |
| `db backup --output` | 使用 SQLite 在线备份能力生成一致备份 |
| `db restore --input` | 校验备份后恢复；执行前自动备份当前库 |
| `db migrate` | 显式执行待应用 schema 迁移 |

不提供任意 SQL 写入。紧急修复通过 override、重建、迁移或恢复完成，从而保留校验与审计能力。

### 9.5 配置

数据库路径解析优先级从高到低为：

1. 每次调用的 `--db PATH`。
2. 环境变量 `REVIEW_KB_DB`。
3. 配置文件中的 `db_path`。
4. 平台默认数据目录中的 `review-kb/knowledge.db`。

配置命令为 `config show`、`config get` 和 `config set db_path PATH`。`config show` 必须明确显示最终值及其来源。自动化流程建议固定使用环境变量或 `--db`，避免依赖执行用户的隐式环境。

## 10. 统一 JSON 协议

所有命令仅向 stdout 输出一个 JSON 文档。日志和诊断信息写入 stderr，不污染协议输出。

成功响应：

```json
{
  "ok": true,
  "data": {},
  "warnings": [],
  "meta": {
    "command": "prepare",
    "schema_version": 1
  }
}
```

失败响应：

```json
{
  "ok": false,
  "error": {
    "code": "CHECKLIST_INVALID",
    "message": "duplicate rule key: SEC-001",
    "details": {
      "path": "/repo/review-checklist.md",
      "line": 42
    }
  },
  "warnings": [],
  "meta": {
    "command": "prepare",
    "schema_version": 1
  }
}
```

错误码至少包括：

- `INVALID_ARGUMENT`
- `CHECKLIST_NOT_FOUND`
- `CHECKLIST_INVALID`
- `PROJECT_NOT_FOUND`
- `RULE_NOT_FOUND`
- `INVALID_SELECTION`
- `KNOWLEDGE_REVISION_MISMATCH`
- `OVERRIDE_CONFLICT`
- `DB_LOCKED`
- `DB_INTEGRITY_ERROR`
- `DB_SCHEMA_UNSUPPORTED`
- `BACKUP_INVALID`

进程退出码按类别稳定映射：`0` 成功、`2` 参数或输入错误、`3` 数据不存在、`4` 冲突、`5` 数据库错误、`1` 未分类内部错误。

## 11. SQLite 数据模型

### 11.1 `projects`

- `project_id TEXT PRIMARY KEY`
- `project_name TEXT NOT NULL`
- `checklist_path TEXT NOT NULL`
- `schema_version INTEGER NOT NULL`
- `checklist_version TEXT NOT NULL`
- `content_hash TEXT NOT NULL`
- `knowledge_revision TEXT NOT NULL`
- `source_mtime_ns INTEGER NULL`：仅用于诊断，不作为一致性依据
- `description_json TEXT NOT NULL`
- `created_at TEXT NOT NULL`
- `updated_at TEXT NOT NULL`

### 11.2 `rules`

- `project_id TEXT NOT NULL`
- `rule_key TEXT NOT NULL`
- `ordinal INTEGER NOT NULL`
- `summary TEXT NOT NULL`
- `content TEXT NOT NULL`
- `tags_json TEXT NOT NULL`
- `paths_json TEXT NOT NULL`
- `languages_json TEXT NOT NULL`
- `source_rule_hash TEXT NOT NULL`
- 主键：`(project_id, rule_key)`
- 唯一约束：`(project_id, ordinal)`
- 外键：`project_id → projects.project_id ON DELETE CASCADE`

### 11.3 `rule_overrides`

- `project_id TEXT NOT NULL`
- `rule_key TEXT NOT NULL`
- 各可覆盖字段的 nullable 值
- `base_source_rule_hash TEXT NOT NULL`
- `status TEXT NOT NULL`：`active/conflict/disabled`
- `reason TEXT NOT NULL`
- `created_at TEXT NOT NULL`
- `updated_at TEXT NOT NULL`
- 主键：`(project_id, rule_key)`
- 外键：`(project_id, rule_key) → rules(...) ON DELETE CASCADE`

### 11.4 `sync_history`

- 同步 ID、project ID、动作、旧/新版本、旧/新哈希、结果、警告、错误码、时间。
- 不保存规则正文副本，避免数据库无界增长；数据库备份承担完整历史恢复职责。

### 11.5 `audit_log`

- 记录 override、restore、migration 等人工运维动作。
- 包含动作类型、目标、原因、变化摘要和时间。
- 第一版为本地审计，不提供不可抵赖保证。

数据库启用外键约束。JSON 字段写入前规范化并在读取时校验。schema 迁移单独版本化，不与 checklist 的 `schema_version` 混用。

## 12. 同步与 Override 冲突规则

同步时逐条比较 `source_rule_hash`：

- 无 override：直接采用新 source rule。
- 有 active override，且对应 source rule 未变化：保留 override。
- 有 active override，但对应 source rule 已变化或被删除：标记 conflict，整个同步不提交。
- 新增的 source rule：正常导入。
- 删除的无 override source rule：正常删除。

发生冲突时，`prepare` 返回 `OVERRIDE_CONFLICT`，旧快照保持完整但不作为本次 prepare 的成功结果。维护者必须通过 `overrides show` 查看差异，并显式选择保留 override 或接受 source。这样不会静默丢失紧急修复，也不会悄悄使用已知过期的规则。

## 13. 一致性、并发和故障处理

- SQLite 使用 WAL 模式和有限 busy timeout，以支持多个并发只读检视进程。
- 同一数据库的写操作由 SQLite 事务串行化。
- `prepare` 在事务提交前再次核对目标项目当前哈希，避免并发刷新覆盖较新的结果。
- `rules get` 在同一只读事务中校验 knowledge revision 并读取全部规则，避免校验后发生快照切换。
- checklist 在读取过程中变化时，重新读取一次；连续变化则返回输入不稳定错误。
- 所有重建先在内存中完成解析和校验，再开启短写事务。
- 不存在数据库时由首次写命令初始化；只读命令不得隐式创建数据库。
- 数据库 schema 高于当前 CLI 支持版本时拒绝打开，避免旧客户端破坏新数据。
- restore 前验证 SQLite 文件、schema 兼容性和完整性；恢复采用临时文件与原子替换。

## 14. 安全边界

- checklist 被视为不可信文本，仅解析 Markdown 和 YAML，不执行其中内容。
- YAML 解析必须使用 safe loader。
- 路径参数规范化，但 CLI 不限制 checklist 必须位于当前工作目录，便于自动化显式指定。
- 错误输出不得包含数据库中不相关项目的规则正文。
- `db restore`、`rebuild`、override 写操作必须明确指定目标数据库和项目。
- 数据库文件权限遵循创建进程 umask；文档建议仅授予检视用户访问权限。

## 15. 可观测性与诊断

`prepare` 的 `meta` 额外返回：

- `project_id`
- `db_path`
- `knowledge_status`：`created/refreshed/reused`
- `checklist_version`
- `content_hash`
- `knowledge_revision`
- `rule_count`

内容变化但版本未更新时返回稳定警告码 `CONTENT_CHANGED_WITHOUT_VERSION_BUMP`。项目名称变化返回 `PROJECT_METADATA_UPDATED`，只更新元数据，不重建规则。

## 16. 测试策略

### 16.1 Parser 单元测试

- 正常文件、空文件、无 Front Matter、未知 schema。
- 重复/非法 key、缺少概要、空正文、未知字段和错误 YAML 类型。
- Markdown 正文、Unicode、换行规范化及稳定哈希。

### 16.2 Description 单元测试

- 固定输入生成逐字节稳定 JSON。
- 顺序、数组去重、精确 key 和 override 后有效字段。
- knowledge revision 对有效规则变化敏感，对项目改名和运行时字段变化不敏感。
- 项目改名只改变项目元数据。

### 16.3 Repository 集成测试

- 初始化、迁移、外键、事务回滚和完整性检查。
- 并发读取、并发 prepare、锁超时。
- 备份和恢复往返一致性。

### 16.4 业务流程测试

- 首次 prepare 构建。
- 相同版本和哈希复用。
- 正常版本升级。
- 内容变化但版本未变化时刷新并告警。
- 项目改名不重建。
- 批量规则读取保持输入顺序且缺失 key 整体失败。
- 选择单错误返回候选建议但不自动纠正，revision 变化强制重新 prepare。
- 仅大小写不同的重复 source key 在导入阶段失败。
- override 设置、保留、冲突和两种解决路径。
- 解析或写入失败时旧快照不受损。

### 16.5 CLI 契约测试

- 每个命令的成功/失败 JSON schema。
- stdout 无日志污染，stderr 可诊断。
- 稳定错误码和进程退出码。
- 数据库路径四级优先级。

## 17. 实施分期

### 阶段一：核心闭环

- Python CLI 工程骨架和配置。
- Checklist parser 与校验。
- SQLite schema 和迁移框架。
- `prepare`、`status`、`description get`、`rules list/get/search`。
- 统一 JSON 和核心测试。

完成后即可接入 MR 检视流程。

### 阶段二：离线运维

- 项目查询。
- Override 生命周期和冲突处理。
- `db info/check/query/backup/restore/migrate`。
- 审计和同步历史。

### 阶段三：稳健性增强

- 并发与故障注入测试。
- 大型 checklist 性能测试。
- 数据库和 CLI 契约兼容性文档。
- 根据真实检视流程反馈扩展筛选字段，不提前加入模型检索。

## 18. 验收标准

第一版核心闭环满足以下条件即可接入试运行：

1. 同一个 checklist 在相同输入下生成逐字节一致的 description。
2. 一个数据库可存储 rule key 重复但 project ID 不同的多个项目。
3. 首次 prepare 自动构建，后续相同版本和哈希直接复用。
4. 版本或内容变化会刷新；内容变化未升版本会产生明确警告。
5. 项目改名只更新展示元数据。
6. Agent 能先取得全部规则概要，再一次批量取得选中规则正文。
7. Agent 使用包含 project ID、knowledge revision 和 key 数组的结构化选择单；错误 key 不会触发错误规则的自动加载。
8. 任意解析、冲突或数据库写入失败都不会暴露半更新数据。
9. 所有 stdout 响应符合统一 JSON 协议，错误码和退出码稳定。
10. 数据库路径可通过参数、环境变量或配置文件指定，并能显示最终来源。
11. 字面量查询能够定位 key、概要、正文和筛选字段中的内容。
12. knowledge revision 变化后，旧选择单不能读取新快照中的规则。
13. 无效 key 不会触发模糊自动替换，错误响应提供候选以便 Agent 修正重试。

## 19. 已确认的设计决策

- 使用用户指定路径的单个 SQLite 数据库管理多个项目。
- CodeHub 项目上下文由调用方传入，CLI 第一版不访问 CodeHub API。
- CodeHub 稳定仓库 ID 是项目身份；项目名称不是主键。
- 规则唯一性采用 `(project_id, rule_key)`。
- 同时使用声明版本和内容 SHA-256 判断同步状态。
- `prepare` 自动完成检查与必要重建。
- CLI 不调用模型，description 完全确定性生成。
- 规则支持可选 tags、paths 和 languages，第一版只作为筛选提示。
- 支持一次批量读取多个 key。
- Agent 通过绑定 knowledge revision 的 JSON 选择单读取规则，失败时显式修正或重新 prepare。
- CLI 对选择单执行全量严格校验，不自动纠正 key，也不返回部分规则正文。
- 所有命令统一 JSON 输出。
- 紧急修改使用可审计 override，不直接覆盖 source rule。
