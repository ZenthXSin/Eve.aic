# Mindustry 知识库结构总览

> 最后更新：2026-06-15

## 知识库架构

```
knowledge/mindustry/
├── structure-overview.md    # 本页：知识库总结构
├── project-structure.md     # 项目源码结构
├── core-packages.md         # 核心包职责
├── arc-framework.md         # Arc框架概览
├── key-classes.md           # 关键类速查
│
├── 核心系统（源码验证）
│   ├── content-system.md        # 内容系统
│   ├── unit-system.md           # 单位系统
│   ├── block-building-system.md # 方块/建筑系统
│   ├── world-system.md          # 世界系统
│   ├── entity-ecs-system.md     # 实体/ECS系统
│   ├── ai-system.md             # AI系统
│   ├── logic-system.md          # 逻辑处理器
│   ├── events-system.md         # 事件系统
│   ├── status-ability-system.md # 状态/能力系统
│   ├── research-tech-tree-system.md # 科技树
│   ├── save-load-system.md      # 存档系统
│   ├── networking-system.md     # 网络系统
│   ├── campaign-system.md       # 战役系统
│   ├── modding-api.md           # Mod API
│   └── vars-system.md           # Vars全局状态
│
├── Vars子系统索引
│   ├── vars-index.md
│   ├── vars-content.md
│   ├── vars-world.md
│   ├── vars-game.md
│   ├── vars-control.md
│   ├── vars-ui.md
│   ├── vars-renderer.md
│   ├── vars-network.md
│   ├── vars-input.md
│   ├── vars-settings.md
│   ├── vars-platform.md
│   ├── vars-services.md
│   └── vars-audio.md
│
└── 专题补充
    ├── entity-ecs-detail.md
    └── save-load-detail.md
```

## 统计

| 指标 | 数值 |
|------|------|
| 知识页总数 | 48 |
| 源码验证页 | 15 |
| 硬性规则 | ~310条 |
| 验证源码行数 | ~35000行 |

## 硬性输出规则

1. 输出 Mindustry 内容前必须先查知识库或源码
2. 未经验证不能输出
3. 最终答复前必须调用至少一个工具
4. 非必要回复20字内，最大150字

## 关联

- [Project Structure](project-structure.md)
- [Key Classes Analysis](key-classes.md)
- [Vars System](vars-system.md)
