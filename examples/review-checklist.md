---
schema_version: 1
checklist_version: "2026.07.1"
global_description: |-
  检查本项目的输入安全、事务边界和错误处理。先根据 MR 变更选择相关规则 key，
  再加载完整规则；不要仅依据概要直接给出检视结论。
---

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

禁止使用字符串拼接构造 SQL。确认查询通过数据库驱动提供的参数绑定接口执行。
动态表名、排序字段等无法直接绑定的位置必须使用服务端白名单限制。

## DB-004

```yaml review-rule
summary: 检查事务边界是否覆盖完整业务操作
tags:
  - database
  - consistency
paths:
  - "src/**/services/*.py"
languages:
  - python
```

### 检视要求

确认同一业务操作中的相关写入处于同一事务，失败路径能够完整回滚。
网络调用和其他不可控的长耗时操作不应被包含在数据库长事务中。

