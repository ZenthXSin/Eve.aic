# 自杀单位配置

> Source: `源码和示例/Mindustry/core/src/mindustry/type/Weapon.java`、`源码和示例/Mindustry/core/src/mindustry/entities/bullet/BulletType.java`

## 三件套

自杀单位（如原版 Eruptor、Horizon 的炸弹攻击）由三个独立字段组合实现：

| 字段 | 位置 | 作用 |
|------|------|------|
| `Weapon.shootOnDeath = true` | `Weapon.java:157` | 单位死亡时触发该武器的 `shoot()` |
| `BulletType.killShooter = true` | `BulletType.java:105` | 子弹命中时杀死发射者 |
| `SuicideAI` | `ai/types/SuicideAI.java` | AI 冲向敌人，进入射程后贴近+射击 |

## 使用示例（原版）

- **Reticle** (`UnitTypes.java:664`): `shootOnDeath = true`
- **Horizon 炸弹** (`UnitTypes.java:684`): `killShooter = true`
- 注意：`shootOnDeath` 和 `killShooter` 可以独立使用，不强制捆绑