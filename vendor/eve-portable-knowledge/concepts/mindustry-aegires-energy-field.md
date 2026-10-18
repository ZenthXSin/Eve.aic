# Aegires 能量场绘制

> Source: Conversation and local source inspection of `mindustry.content.UnitTypes` and `mindustry.entities.abilities.EnergyFieldAbility`

## Key Points

- Aegires is defined in `mindustry.content.UnitTypes` and uses `new EnergyFieldAbility(40f, 65f, 180f)`.
- The visible "energy ball" is not a model; it is drawn in `mindustry.entities.abilities.EnergyFieldAbility.draw(Unit unit)`.
- The draw routine stacks two filled circles:
  - `Fill.circle(rx, ry, orbRadius);`
  - `Fill.circle(rx, ry, orbRadius / 2f);`
- Around the orb, it draws rotating arcs with `Lines.arc(...)` to make the field look alive.
- The effect is finished with `Drawf.light(...)` so the field glows.

## Relevant Source

- `源码和示例/Mindustry/core/src/mindustry/content/UnitTypes.java`
- `源码和示例/Mindustry/core/src/mindustry/entities/abilities/EnergyFieldAbility.java`

## Related

- [Arc Draw 如何在屏幕上画东西](../concepts/arc-draw-screen-rendering.md) — explains the Arc drawing stack behind `Fill.circle` and `Lines.arc`
- [Mindustry 自定义 Shader 接入](../concepts/mindustry-custom-shader.md) — useful if you want to make the orb look like a shader effect instead of a pure 2D draw
