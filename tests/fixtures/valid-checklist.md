---
schema_version: 1
checklist_version: "2026.07.1"
global_description: |-
  检查本项目的安全性、事务边界、兼容性和可观测性。
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

禁止使用字符串拼接构造 SQL。确认查询使用参数绑定。

## DB-004

```yaml review-rule
summary: 检查事务边界是否覆盖完整业务操作
tags:
  - database
paths: []
languages:
  - python
```

确认失败路径会回滚事务，且外部调用不会被包含在长事务中。

