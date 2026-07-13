# review-kb：安装与 Agent 接入教程

`review-kb` 是一个本地 Code Review 规则知识库 CLI。它把项目中的 `review-checklist.md` 确定性地导入 SQLite，先向 Agent 提供规则概要，再根据 Agent 选择的 key 批量返回完整检视规则。

CLI 不调用模型、不使用向量检索，也不访问 CodeHub API。CodeHub 项目 ID、项目名称和 checklist 路径由现有 MR 检视流程传入。

## 运行时：Python 与 Rust（双实现，可互换）

本仓库提供两个**字节兼容、可互换**的实现，调用方把 `review-kb` 从一个换成另一个无需改动任何参数或脚本：

- **Python**：`src/review_kb/`，经 `uv` 运行(见第 2 节)。适合环境里已有 Python / `uv` 的场景。
- **Rust**：`rust/`，单一二进制、SQLite 已静态编入、无 Python 运行时依赖。适合需要单一可执行文件或更冷启动快的执行机。构建与安装见 [`rust/README.md`](rust/README.md)。

两者共享同一套契约：同一 SQLite 数据库文件(可互相读写)、同一 JSON 信封、同一退出码、同一三处 SHA-256 哈希、同一 checklist 格式。跨二进制一致性由自动化门禁覆盖，从仓库根执行：

```bash
make test       # Python pytest + Rust cargo test 全量
make compat     # 跨二进制 stdout/退出码逐字节比对等一致性门禁
```

> **PATH 冲突**：两个实现的二进制**同名,都叫 `review-kb`**。若同时安装,实际命中哪个取决于 `PATH` 顺序(`which -a review-kb` 可查)。生产环境建议只保留其一,或在脚本里用绝对路径调用。两处仅影响诊断文本、不影响程序化契约的差异见 [`rust/README.md`](rust/README.md) 第 5 节。

下文(第 1 节起)以 Python 版为例讲解命令用法;Rust 版参数与 JSON 协议完全相同。

## 1. 环境要求

- Python 3.10 或更高版本
- 推荐使用 [uv](https://docs.astral.sh/uv/)
- 本地可写的 SQLite 数据库目录
- 每个接入项目包含固定格式的 `review-checklist.md`

## 2. 安装 CLI

### 2.1 在源码目录运行

适用于开发和联调：

```bash
uv sync
uv run review-kb --help
```

后续命令均可使用 `uv run review-kb`。

### 2.2 构建并安装到执行机

```bash
uv build
uv tool install ./dist/code_review_knowledge-0.1.0-py3-none-any.whl
review-kb --help
```

如果版本发生变化，请使用 `dist/` 中实际生成的 wheel 文件名。安装后，自动化脚本直接调用 `review-kb`。

### 2.3 验证安装

帮助信息应包含以下核心命令：

```text
prepare  status  sync  rebuild
description  projects  rules  overrides
db  config
```

## 3. 配置数据库路径

数据库路径优先级为：

```text
--db > REVIEW_KB_DB > 配置文件 > 平台默认目录
```

自动化环境推荐固定设置环境变量：

```bash
export REVIEW_KB_DB=/data/code-review/review-kb.db
review-kb config show
```

也可以写入配置文件：

```bash
export REVIEW_KB_CONFIG=/data/code-review/review-kb.toml
review-kb config set db_path /data/code-review/review-kb.db
review-kb config get db_path
```

`config show` 或 `config get db_path` 会返回最终路径及其来源。生产环境建议使用绝对路径，并保证数据库目录只对检视执行用户开放写权限。

## 4. 在项目中编写 checklist

在待检视项目根目录创建 `review-checklist.md`：

````markdown
---
schema_version: 1
checklist_version: "2026.07.1"
global_description: |-
  检查本项目的安全性、事务边界、兼容性和可观测性。
---

## SEC-001

```yaml review-rule
summary: 检查外部输入是否在进入 SQL 前完成参数化处理
tags: [security, database]
paths: ["src/**/*.py"]
languages: [python]
```

### 检视要求

禁止使用字符串拼接构造 SQL。确认查询通过数据库驱动提供的参数绑定接口执行。
动态表名和排序字段必须经过白名单限制。
````

要求：

- `schema_version` 当前固定为 `1`。
- `checklist_version` 是项目维护的版本字符串。
- `global_description` 是项目级检视说明。
- 二级标题是稳定的规则 key，项目内唯一且不能只通过大小写区分。
- `summary` 和规则正文必填。
- `tags`、`paths`、`languages` 可选，仅用于帮助 Agent 筛选。
- 修改规则内容时应同步更新 `checklist_version`。

可直接参考 [示例 checklist](examples/review-checklist.md)。

## 5. MR 检视接入流程

完整调用链为：

```text
prepare
  → 把 description 和 selection_context 交给 Agent
  → Agent 返回结构化 key 选择单
  → rules get 批量取得完整规则
  → Agent 使用完整规则检视 MR
```

### 5.1 准备项目知识库

从 CodeHub 检视上下文取得稳定仓库 ID 和展示名称：

```bash
review-kb prepare \
  --project-id "codehub-123" \
  --project-name "payment-service" \
  --checklist "/workspace/payment-service/review-checklist.md"
```

`prepare` 会自动完成以下操作：

1. 解析并校验 checklist。
2. 查询该 `project_id` 是否已经存在。
3. 比较 checklist 声明版本和文件 SHA-256。
4. 首次接入时创建知识库，内容变化时原子刷新，未变化时直接复用。
5. 返回 description 和绑定当前有效规则的 `knowledge_revision`。

关键返回数据：

```json
{
  "description": {
    "global_description": "项目级检视说明",
    "rules": [
      {
        "key": "SEC-001",
        "summary": "检查外部输入是否完成安全处理",
        "tags": ["security"],
        "paths": ["src/**/*.py"],
        "languages": ["python"]
      }
    ]
  },
  "selection_context": {
    "project_id": "codehub-123",
    "knowledge_revision": "sha256:..."
  }
}
```

### 5.2 约束 Agent 的输出

传给 Agent 的输入应包含：

- 当前 MR 的 diff 和文件列表；
- `data.description.global_description`；
- `data.description.rules`；
- `data.selection_context`。

建议在 Agent 提示中加入以下约束：

> 根据 MR 变更选择需要执行的检视规则。只能从 `description.rules[].key` 原样复制 key，不得修改或虚构 key。保留输入中的 `project_id` 和 `knowledge_revision`，只输出一个 JSON 对象，不要输出 Markdown 或解释文字。

Agent 必须返回：

```json
{
  "project_id": "codehub-123",
  "knowledge_revision": "sha256:...",
  "keys": ["SEC-001", "DB-004"]
}
```

不要让 Agent 返回逗号分隔字符串，也不要给 Agent 同时暴露另一套带项目前缀的 key。

### 5.3 批量取得完整规则

把 Agent JSON 输出保存为 `agent-selection.json`，然后执行：

```bash
review-kb rules get --input agent-selection.json
```

也可以通过 stdin 传入：

```bash
review-kb rules get --input - < agent-selection.json
```

只有 project ID、revision 和全部 key 都校验通过后，CLI 才返回完整规则正文。Agent 必须使用返回的 `data.rules[].content` 执行检视，不能只依据 summary 检视。

## 6. 调用方错误恢复

调用方必须同时检查进程退出码和 JSON 的 `ok` 字段。

| 错误 code | 处理方式 |
|---|---|
| `RULE_NOT_FOUND` | 把 `not_found` 和 `suggestions` 返回 Agent，允许显式修正一次 |
| `KNOWLEDGE_REVISION_MISMATCH` | 丢弃旧选择单，重新执行 `prepare → Agent 筛选 → rules get` |
| `OVERRIDE_CONFLICT` | 停止检视规则加载，由维护者解决本地覆盖冲突 |
| `CHECKLIST_INVALID` | 修复 checklist 格式或字段后重新执行 prepare |
| `DB_LOCKED` / `DB_INTEGRITY_ERROR` | 停止检视并进入数据库诊断流程 |

禁止自动采用模糊匹配候选，也不能忽略失败 key 后使用部分规则继续检视。

进程退出码：

```text
0 成功
1 未分类内部错误
2 参数、checklist 或选择单错误
3 项目或规则不存在
4 revision 或 override 冲突
5 数据库错误
```

## 7. Python 调用示例

```python
import json
import subprocess


def review_kb(*args: str, input_payload: dict | None = None) -> dict:
    result = subprocess.run(
        ["review-kb", *args],
        input=json.dumps(input_payload, ensure_ascii=False) if input_payload else None,
        text=True,
        capture_output=True,
        check=False,
    )
    payload = json.loads(result.stdout)
    if result.returncode != 0 or not payload["ok"]:
        raise RuntimeError(payload["error"])
    return payload["data"]


prepared = review_kb(
    "prepare",
    "--project-id", "codehub-123",
    "--project-name", "payment-service",
    "--checklist", "/workspace/payment-service/review-checklist.md",
)

# 把 prepared["description"] 和 prepared["selection_context"] 交给 Agent。
# agent_selection 是 Agent 严格按照约束返回的 JSON 对象。
agent_selection = {**prepared["selection_context"], "keys": ["SEC-001"]}
rules = review_kb("rules", "get", "--input", "-", input_payload=agent_selection)
```

生产代码应根据上一节的错误 code 实现分支处理，不能统一转成普通异常后无条件重试。

## 8. 离线查询和规则修复

```bash
# 项目与 description
review-kb projects list
review-kb projects show --project-id codehub-123
review-kb description get --project-id codehub-123

# 规则概要和字面量查询
review-kb rules list --project-id codehub-123
review-kb rules search --project-id codehub-123 --query "事务"

# 数据库受控查询
review-kb db query --view rules --project-id codehub-123 --query "security"
review-kb db info
review-kb db check
```

紧急修复通过 override 完成，不直接修改 source rule：

```bash
review-kb overrides set --input - <<'JSON'
{
  "project_id": "codehub-123",
  "key": "SEC-001",
  "reason": "安全事件应急加强",
  "content": "所有 SQL 必须使用参数绑定，并检查动态标识符白名单。"
}
JSON
```

override 会改变 `knowledge_revision`，已有 Agent 选择单必须重新生成。解除 override：

```bash
review-kb overrides unset \
  --project-id codehub-123 \
  --key SEC-001 \
  --reason "源 checklist 已完成修复"
```

冲突处理和 `keep-override` 操作详见[完整接入与运维指南](docs/integration-guide.md)。

## 9. 备份、恢复和升级

```bash
# 备份文件必须不存在
review-kb db backup --output /backup/review-kb-20260705.db

# 恢复前 CLI 会自动备份当前数据库
review-kb db restore --input /backup/review-kb-20260705.db

# 升级 CLI 后应用数据库 migration
review-kb db migrate
review-kb db check
```

执行恢复期间必须停止其他写入该数据库的进程。

## 10. 上线前检查

- `review-kb --help` 可以正常执行。
- `review-kb db check` 返回 `integrity: ["ok"]`。
- 自动化环境使用固定的数据库绝对路径。
- CodeHub 稳定仓库 ID 正确映射到 `--project-id`。
- checklist 能通过首次 `prepare` 校验。
- Agent 只输出结构化选择单，并原样复制 key。
- 调用方实现 key 修正和 revision 失效后的完整重启。
- 调用方在任意知识库错误时停止规则加载，不使用部分结果。
- 已验证数据库备份能够恢复。

更多命令和冲突处理示例参见[完整接入与运维指南](docs/integration-guide.md)。
