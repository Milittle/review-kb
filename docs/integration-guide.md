# `review-kb` 接入 MR 检视流程

本文说明如何把本地规则知识库接入现有 Agent 检视流程。CLI 只负责确定性构建和查询；Agent 负责根据 MR 变更选择规则并执行检视。

## 1. 安装与数据库路径

开发环境运行：

```bash
uv sync
uv run review-kb --help
```

构建可分发包并安装到检视执行机：

```bash
uv build
uv tool install ./dist/code_review_knowledge-0.1.0-py3-none-any.whl
review-kb --help
```

接入脚本在安装后应直接调用 `review-kb`。下文中的 `uv run review-kb` 用于源码工作区调试，两种调用方式的参数和 JSON 协议完全相同。

自动化环境应固定数据库路径，避免不同执行用户读取不同默认库：

```bash
export REVIEW_KB_DB=/data/code-review/review-kb.db
```

也可以在每次调用时传 `--db /data/code-review/review-kb.db`。优先级是 `--db`、`REVIEW_KB_DB`、配置文件、平台默认目录。通过以下命令确认最终路径及其来源：

```bash
uv run review-kb config show
```

也可以持久化配置。自动化测试或容器中可通过 `REVIEW_KB_CONFIG` 隔离配置文件：

```bash
export REVIEW_KB_CONFIG=/data/code-review/review-kb.toml
uv run review-kb config set db_path /data/code-review/review-kb.db
uv run review-kb config get db_path
```

## 2. 维护项目 checklist

项目根目录维护固定格式的 `review-checklist.md`。可参考 [示例 checklist](../examples/review-checklist.md)。

必需的文件级字段：

```yaml
---
schema_version: 1
checklist_version: "2026.07.1"
global_description: |-
  本项目的全局检视说明。
---
```

每条规则使用二级标题作为稳定 key，紧跟一个 `yaml review-rule` 元数据块，再填写完整 Markdown 规则正文：

````markdown
## SEC-001

```yaml review-rule
summary: 检查外部输入是否完成安全处理
tags: [security]
paths: ["src/**/*.py"]
languages: [python]
```

这里填写 Agent 实际执行检视时使用的完整规则。
````

同一项目的 key 必须唯一，且不能只通过大小写区分。修改内容时应同时更新 `checklist_version`；忘记更新版本不会阻止刷新，但 CLI 会返回 `CONTENT_CHANGED_WITHOUT_VERSION_BUMP` 警告。

## 3. MR 开始时执行 prepare

检视流程从 CodeHub 上下文取得稳定仓库 ID 和当前展示名称：

```bash
uv run review-kb prepare \
  --project-id "codehub-123" \
  --project-name "payment-service" \
  --checklist "/workspace/payment-service/review-checklist.md"
```

首次调用返回 `knowledge_status: created`，内容变化返回 `refreshed`，数据一致返回 `reused`。成功数据中的两个关键对象是：

```json
{
  "description": {
    "global_description": "...",
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

项目改名只更新 `project_name`，不会改变 `knowledge_revision` 或重建规则。

## 4. 约束 Agent 的规则选择输出

传给 Agent：

- MR 的变更内容；
- `description.global_description`；
- `description.rules`；
- `selection_context`；
- 以下输出约束。

建议在 Agent 提示中明确要求：

> 只从 `description.rules[].key` 原样复制适用于本 MR 的 key。返回一个 JSON 对象，保留给定的 `project_id` 和 `knowledge_revision`，并把选中的 key 放入 `keys` 数组。不要改写 key，不要返回逗号分隔字符串，不要虚构规则。

Agent 的输出必须是：

```json
{
  "project_id": "codehub-123",
  "knowledge_revision": "sha256:...",
  "keys": ["SEC-001", "DB-004"]
}
```

不要同时给 Agent 暴露另一套带项目前缀的 key。项目隔离已经由 `project_id` 完成。

## 5. 批量加载完整规则

把 Agent 输出原样写入 stdin：

```bash
uv run review-kb rules get --input - < agent-selection.json
```

所有 key 都通过校验后，CLI 才按输入顺序返回完整规则正文。调用方必须使用正文执行检视，不能只使用 summary。

不要把 Agent 输出转换成逗号分隔参数。JSON 数组可以避免 shell 引号、空白和拆分错误。

## 6. key 获取失败后的恢复

错误 key 不会被 CLI 自动替换，也不会返回部分规则。示例：

```json
{
  "ok": false,
  "error": {
    "code": "RULE_NOT_FOUND",
    "message": "one or more selected rule keys do not exist",
    "details": {
      "not_found": ["SEC-01"],
      "suggestions": {
        "SEC-01": ["SEC-001", "DB-004"]
      }
    }
  }
}
```

恢复流程：

1. 将 `not_found` 和对应 `suggestions` 返回给 Agent。
2. 明确要求 Agent 从候选中确认正确 key，而不是让程序自动采用第一项。
3. 用修正后的完整选择单重新调用一次 `rules get`。
4. 第二次仍失败时停止规则加载，报告集成错误，不能忽略缺失 key 继续检视。

禁止自动模糊匹配，因为相似 key 可能表达完全不同的检视规则。

## 7. knowledge revision 变化后的恢复

如果 prepare、规则覆盖或其他更新改变了有效知识，旧选择单会返回：

```json
{
  "ok": false,
  "error": {
    "code": "KNOWLEDGE_REVISION_MISMATCH",
    "message": "knowledge revision changed; run prepare and select rules again"
  }
}
```

此时必须丢弃旧选择单，重新执行完整链路：

```text
prepare → 把新 description 交给 Agent → 生成新选择单 → rules get
```

不能只替换 revision 后继续使用旧 key，因为新版本可能增加、删除或改变规则适用范围。

## 8. 紧急覆盖规则

当源 checklist 暂时无法及时修改时，可以写入有原因记录的本地 override。源规则不会被直接修改：

```bash
uv run review-kb overrides set --input - <<'JSON'
{
  "project_id": "codehub-123",
  "key": "SEC-001",
  "reason": "安全事件应急加强规则",
  "summary": "加强 SQL 输入安全检查",
  "content": "所有 SQL 必须使用参数绑定，并检查动态标识符白名单。"
}
JSON
```

可覆盖 `summary`、`content`、`tags`、`paths` 和 `languages` 中的任意组合。操作成功后 `knowledge_revision` 会变化，已有 Agent 选择单必须丢弃并重新生成。

查询覆盖和源值：

```bash
uv run review-kb overrides list --project-id codehub-123
uv run review-kb overrides show --project-id codehub-123 --key SEC-001
```

应急结束后禁用覆盖：

```bash
uv run review-kb overrides unset \
  --project-id codehub-123 \
  --key SEC-001 \
  --reason "源 checklist 已完成修复"
```

如果被覆盖的 source rule 本身发生变化，下一次 prepare 返回 `OVERRIDE_CONFLICT` 并保留旧快照，防止任何一方被静默覆盖。先用 `overrides show` 比较内容，然后显式解决：

```bash
# 接受仓库中的新 source rule，禁用本地 override，然后完成刷新
uv run review-kb overrides resolve \
  --project-id codehub-123 \
  --key SEC-001 \
  --strategy accept-source \
  --checklist /workspace/payment-service/review-checklist.md \
  --reason "仓库规则已经包含应急修复"

# 保留本地 override，并把它重新绑定到仓库中的新 source rule
uv run review-kb overrides resolve \
  --project-id codehub-123 \
  --key SEC-001 \
  --strategy keep-override \
  --checklist /workspace/payment-service/review-checklist.md \
  --reason "新源规则仍需叠加本地限制"
```

`keep-override` 要求新 checklist 中仍存在该 key。任一解决操作都会刷新知识库并改变 revision；必须重新让 Agent 选择规则。

## 9. 离线定位命令

```bash
# 查看项目和当前版本
uv run review-kb projects list
uv run review-kb projects show --project-id codehub-123

# 查看 description 和规则概要
uv run review-kb description get --project-id codehub-123
uv run review-kb rules list --project-id codehub-123

# 在 key、概要、正文和筛选字段中执行字面量查询
uv run review-kb rules search --project-id codehub-123 --query "事务"

# 查看数据库统计和执行完整性检查
uv run review-kb db info
uv run review-kb db check

# 查询受控视图；view 只能是 projects/rules/overrides/sync_history/audit_log
uv run review-kb db query \
  --view rules \
  --project-id codehub-123 \
  --query "事务" \
  --limit 100
```

这些命令统一返回单个 JSON 文档。stdout 供程序消费，进程退出码用于分类：`0` 成功、`2` 输入错误、`3` 数据不存在、`4` revision 冲突、`5` 数据库错误。

## 10. 显式同步、重建与数据库维护

日常 MR 流程只需要 `prepare`。以下命令用于人工运维：

```bash
# 与 prepare 相同地检查并按需同步
uv run review-kb sync \
  --project-id codehub-123 \
  --project-name payment-service \
  --checklist /workspace/payment-service/review-checklist.md

# 即使版本和哈希未变化，也强制重新解析并原子写入
uv run review-kb rebuild \
  --project-id codehub-123 \
  --project-name payment-service \
  --checklist /workspace/payment-service/review-checklist.md

# 显式应用数据库 schema migration
uv run review-kb db migrate
```

执行升级或批量修复前先备份：

```bash
uv run review-kb db backup --output /backup/review-kb-20260705.db
uv run review-kb db restore --input /backup/review-kb-20260705.db
uv run review-kb db check
```

备份输出文件不得已存在，避免误覆盖。恢复前 CLI 会为当前数据库生成带时间戳的 safety backup，并在 JSON 中返回其路径。恢复操作期间必须停止其他写入该数据库的进程。

## 11. 推荐的调用状态机

```text
开始 MR 检视
  → prepare
  → Agent 根据 description 选择 key
  → rules get
      → 成功：使用全部规则正文检视 MR
      → RULE_NOT_FOUND：让 Agent 显式修正一次
      → KNOWLEDGE_REVISION_MISMATCH：回到 prepare
      → 其他错误：停止加载并报告
```

关键原则是：精确 key、结构化传输、全有或全无、revision 绑定，以及错误后的显式恢复。

## 12. 自动化调用要求

调用方必须同时检查进程退出码和 JSON 的 `ok` 字段：

```python
import json
import subprocess


def call_review_kb(*args: str, input_payload: dict | None = None) -> dict:
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
```

不要从 stderr 解析业务数据；stderr 只用于日志诊断。不要在 `RULE_NOT_FOUND`、revision 冲突或 override 冲突时继续使用部分知识。

## 13. 上线检查清单

- 已在执行机安装 wheel，并确认 `review-kb --help` 正常。
- 使用 `--db` 或 `REVIEW_KB_DB` 固定数据库绝对路径。
- 数据库目录只对检视执行用户开放写权限。
- 每个接入项目都有格式校验通过的 `review-checklist.md`。
- CodeHub 稳定仓库 ID 被传入 `--project-id`，项目名称只用于展示。
- Agent 提示严格约束为复制 `description.rules[].key` 并返回 JSON 选择单。
- 调用方实现 `RULE_NOT_FOUND` 一次纠错和 revision 冲突的完整重启。
- 定期执行 `db backup` 和 `db check`，并验证备份文件能够恢复。
- 升级 CLI 后先执行 `db migrate`，再开始新的 MR 检视任务。
- 日志保留 CLI 退出码、错误 code、project ID 和 revision，但不记录不相关规则正文。
